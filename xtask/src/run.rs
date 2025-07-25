use std::{
    ffi::OsString,
    fmt::Write as _,
    fs::{OpenOptions, copy, create_dir_all},
    io::{BufRead as _, BufReader, Write as _},
    ops::Deref as _,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Output, Stdio},
    sync::{Arc, Mutex},
    thread,
};

use anyhow::{Context as _, Result, anyhow, bail};
use cargo_metadata::{Artifact, CompilerMessage, Message, Target};
use clap::Parser;
use walkdir::WalkDir;
use xtask::{AYA_BUILD_INTEGRATION_BPF, Errors};

#[derive(Parser)]
enum Environment {
    /// Runs the integration tests locally.
    Local {
        /// The command used to wrap your application.
        #[clap(short, long, default_value = "sudo -E")]
        runner: String,
    },
    /// Runs the integration tests in a VM.
    VM {
        /// The cache directory in which to store intermediate artifacts.
        #[clap(long)]
        cache_dir: PathBuf,

        /// The Github API token to use if network requests to Github are made.
        ///
        /// This may be required if Github rate limits are exceeded.
        #[clap(long)]
        github_api_token: Option<String>,

        /// The kernel image and modules to use.
        ///
        /// Format: </path/to/image/vmlinuz>:</path/to/lib/modules>
        ///
        /// You can download some images with:
        ///
        /// wget --accept-regex '.*/linux-image-[0-9\.-]+-cloud-.*-unsigned*' \
        ///   --recursive http://ftp.us.debian.org/debian/pool/main/l/linux/
        ///
        /// You can then extract the images and kernel modules with:
        ///
        /// find . -name '*.deb' -print0 \
        /// | xargs -0 -I {} sh -c "dpkg --fsys-tarfile {} \
        ///   | tar --wildcards --extract '**/boot/*' '**/modules/*' --file -"
        ///
        /// `**/boot/*` is used to extract the kernel image and config.
        ///
        /// `**/modules/*` is used to extract the kernel modules.
        ///
        /// Modules are required since not all parts of the kernel we want to
        /// test are built-in.
        #[clap(required = true, value_parser=parse_image_and_modules)]
        image_and_modules: Vec<(PathBuf, PathBuf)>,
    },
}

pub(crate) fn parse_image_and_modules(s: &str) -> Result<(PathBuf, PathBuf), std::io::Error> {
    let mut parts = s.split(':');
    let image = parts
        .next()
        .ok_or(std::io::ErrorKind::InvalidInput)
        .map(PathBuf::from)?;
    let modules = parts
        .next()
        .ok_or(std::io::ErrorKind::InvalidInput)
        .map(PathBuf::from)?;
    if parts.next().is_some() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    Ok((image, modules))
}

#[derive(Parser)]
pub struct Options {
    #[clap(subcommand)]
    environment: Environment,
    /// Arguments to pass to your application.
    #[clap(global = true, last = true)]
    run_args: Vec<OsString>,
}

pub fn build<F>(target: Option<&str>, f: F) -> Result<Vec<(String, PathBuf)>>
where
    F: FnOnce(&mut Command) -> &mut Command,
{
    // Always use rust-lld in case we're cross-compiling.
    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--message-format=json"]);
    if let Some(target) = target {
        cmd.args(["--target", target]);
    }
    f(&mut cmd);

    let mut child = cmd
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {cmd:?}"))?;
    let Child { stdout, .. } = &mut child;

    let stdout = stdout.take().unwrap();
    let stdout = BufReader::new(stdout);
    let mut executables = Vec::new();
    for message in Message::parse_stream(stdout) {
        #[expect(clippy::collapsible_match)]
        match message.context("valid JSON")? {
            Message::CompilerArtifact(Artifact {
                executable,
                target: Target { name, .. },
                ..
            }) => {
                if let Some(executable) = executable {
                    executables.push((name, executable.into()));
                }
            }
            Message::CompilerMessage(CompilerMessage { message, .. }) => {
                for line in message.rendered.unwrap_or_default().split('\n') {
                    println!("cargo:warning={line}");
                }
            }
            Message::TextLine(line) => {
                println!("{line}");
            }
            _ => {}
        }
    }

    let status = child
        .wait()
        .with_context(|| format!("failed to wait for {cmd:?}"))?;
    if status.code() != Some(0) {
        bail!("{cmd:?} failed: {status:?}")
    }
    Ok(executables)
}

/// Build and run the project.
pub fn run(opts: Options) -> Result<()> {
    let Options {
        environment,
        run_args,
    } = opts;

    type Binary = (String, PathBuf);
    fn binaries(target: Option<&str>) -> Result<Vec<(&str, Vec<Binary>)>> {
        ["dev", "release"]
            .into_iter()
            .map(|profile| {
                let binaries = build(target, |cmd| {
                    cmd.env(AYA_BUILD_INTEGRATION_BPF, "true").args([
                        "--package",
                        "integration-test",
                        "--tests",
                        "--profile",
                        profile,
                    ])
                })?;
                anyhow::Ok((profile, binaries))
            })
            .collect()
    }

    // Use --test-threads=1 to prevent tests from interacting with shared
    // kernel state due to the lack of inter-test isolation.
    let default_args = [OsString::from("--test-threads=1")];
    let run_args = default_args.iter().chain(run_args.iter());

    match environment {
        Environment::Local { runner } => {
            let mut args = runner.trim().split_terminator(' ');
            let runner = args.next().ok_or(anyhow!("no first argument"))?;
            let args = args.collect::<Vec<_>>();

            let binaries = binaries(None)?;

            let mut failures = String::new();
            for (profile, binaries) in binaries {
                for (name, binary) in binaries {
                    let mut cmd = Command::new(runner);
                    cmd.args(args.iter())
                        .arg(binary)
                        .args(run_args.clone())
                        .env("RUST_BACKTRACE", "1")
                        .env("RUST_LOG", "debug");

                    println!("{profile}:{name} running {cmd:?}");

                    let status = cmd
                        .status()
                        .with_context(|| format!("failed to run {cmd:?}"))?;
                    if status.code() != Some(0) {
                        writeln!(&mut failures, "{profile}:{name} failed: {status:?}")
                            .context("String write failed")?
                    }
                }
            }
            if failures.is_empty() {
                Ok(())
            } else {
                Err(anyhow!("failures:\n{}", failures))
            }
        }
        Environment::VM {
            cache_dir,
            github_api_token,
            image_and_modules,
        } => {
            // The user has asked us to run the tests on a VM. This is involved; strap in.
            //
            // We need tools to build the initramfs; we use gen_init_cpio from the Linux repository,
            // taking care to cache it.
            //
            // Then we iterate the kernel images, using the `file` program to guess the target
            // architecture. We then build the init program and our test binaries for that
            // architecture, and use gen_init_cpio to build an initramfs containing the test
            // binaries. We're almost ready to run the VM.
            //
            // We consult our OS, our architecture, and the target architecture to determine if
            // hardware acceleration is available, and then start QEMU with the provided kernel
            // image and the initramfs we built.
            //
            // We consume the output of QEMU, looking for the output of our init program. This is
            // the only way to distinguish success from failure. We batch up the errors across all
            // VM images and report to the user. The end.

            create_dir_all(&cache_dir).context("failed to create cache dir")?;
            let gen_init_cpio = cache_dir.join("gen_init_cpio");
            let etag_path = cache_dir.join("gen_init_cpio.etag");
            {
                let gen_init_cpio_exists = gen_init_cpio.try_exists().with_context(|| {
                    format!("failed to check existence of {}", gen_init_cpio.display())
                })?;
                let etag_path_exists = etag_path.try_exists().with_context(|| {
                    format!("failed to check existence of {}", etag_path.display())
                })?;
                if !gen_init_cpio_exists && etag_path_exists {
                    println!(
                        "cargo:warning=({}).exists()={} != ({})={} (mismatch)",
                        gen_init_cpio.display(),
                        gen_init_cpio_exists,
                        etag_path.display(),
                        etag_path_exists,
                    )
                }
            }

            let gen_init_cpio_source = {
                drop(github_api_token); // Currently unused, but kept around in case we need it in the future.

                let mut curl = Command::new("curl");
                curl.args([
                    "-sfSL",
                    "https://raw.githubusercontent.com/torvalds/linux/master/usr/gen_init_cpio.c",
                ]);
                for arg in ["--etag-compare", "--etag-save"] {
                    curl.arg(arg).arg(&etag_path);
                }

                let Output {
                    status,
                    stdout,
                    stderr,
                } = curl
                    .output()
                    .with_context(|| format!("failed to run {curl:?}"))?;
                if status.code() != Some(0) {
                    bail!("{curl:?} failed: stdout={stdout:?} stderr={stderr:?}")
                }
                stdout
            };

            if !gen_init_cpio_source.is_empty() {
                let mut clang = Command::new("clang");
                clang
                    .args(["-g", "-O2", "-x", "c", "-", "-o"])
                    .arg(&gen_init_cpio)
                    .stdin(Stdio::piped());
                let mut clang_child = clang
                    .spawn()
                    .with_context(|| format!("failed to spawn {clang:?}"))?;

                let Child { stdin, .. } = &mut clang_child;

                let mut stdin = stdin.take().unwrap();
                stdin
                    .write_all(&gen_init_cpio_source)
                    .with_context(|| format!("failed to write to {clang:?} stdin"))?;
                drop(stdin); // Must explicitly close to signal EOF.

                let output = clang_child
                    .wait_with_output()
                    .with_context(|| format!("failed to wait for {clang:?}"))?;
                let Output { status, .. } = &output;
                if status.code() != Some(0) {
                    bail!("{clang:?} failed: {output:?}")
                }
            }

            let mut errors = Vec::new();
            for (kernel_image, modules_dir) in image_and_modules {
                // Guess the guest architecture.
                let mut cmd = Command::new("file");
                let output = cmd
                    .arg("--brief")
                    .arg(&kernel_image)
                    .output()
                    .with_context(|| format!("failed to run {cmd:?}"))?;
                let Output { status, .. } = &output;
                if status.code() != Some(0) {
                    bail!("{cmd:?} failed: {output:?}")
                }
                let Output { stdout, .. } = output;

                // Now parse the output of the file command, which looks something like
                //
                // - Linux kernel ARM64 boot executable Image, little-endian, 4K pages
                //
                // - Linux kernel x86 boot executable bzImage, version 6.1.0-10-cloud-amd64 [..]

                let stdout = String::from_utf8(stdout)
                    .with_context(|| format!("invalid UTF-8 in {cmd:?} stdout"))?;
                let (_, stdout) = stdout
                    .split_once("Linux kernel")
                    .ok_or_else(|| anyhow!("failed to parse {cmd:?} stdout: {stdout}"))?;
                let (guest_arch, _) = stdout
                    .split_once("boot executable")
                    .ok_or_else(|| anyhow!("failed to parse {cmd:?} stdout: {stdout}"))?;
                let guest_arch = guest_arch.trim();

                let (guest_arch, machine, cpu, console) = match guest_arch {
                    "ARM64" => ("aarch64", Some("virt"), Some("max"), "ttyAMA0"),
                    "x86" => ("x86_64", None, None, "ttyS0"),
                    guest_arch => (guest_arch, None, None, "ttyS0"),
                };

                let target = format!("{guest_arch}-unknown-linux-musl");

                let test_distro_args =
                    ["--package", "test-distro", "--release", "--features", "xz2"];
                let test_distro: Vec<(String, PathBuf)> =
                    build(Some(&target), |cmd| cmd.args(test_distro_args))
                        .context("building test-distro package failed")?;

                let binaries = binaries(Some(&target))?;

                let tmp_dir = tempfile::tempdir().context("tempdir failed")?;

                let initrd_image = tmp_dir.path().join("qemu-initramfs.img");
                let initrd_image_file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&initrd_image)
                    .with_context(|| {
                        format!("failed to create {} for writing", initrd_image.display())
                    })?;

                let mut gen_init_cpio = Command::new(&gen_init_cpio);
                let mut gen_init_cpio_child = gen_init_cpio
                    .arg("-")
                    .stdin(Stdio::piped())
                    .stdout(initrd_image_file)
                    .spawn()
                    .with_context(|| format!("failed to spawn {gen_init_cpio:?}"))?;
                let Child { stdin, .. } = &mut gen_init_cpio_child;
                let stdin = Arc::new(stdin.take().unwrap());
                use std::os::unix::ffi::OsStrExt as _;

                // Send input into gen_init_cpio for directories
                //
                // dir  /bin                  755 0 0
                let write_dir = |out_path: &Path| {
                    for bytes in [
                        "dir ".as_bytes(),
                        out_path.as_os_str().as_bytes(),
                        " ".as_bytes(),
                        "755 0 0\n".as_bytes(),
                    ] {
                        stdin.deref().write_all(bytes).expect("write");
                    }
                };

                // Send input into gen_init_cpio for files
                //
                // file /init    path-to-init 755 0 0
                let write_file = |out_path: &Path, in_path: &Path, mode: &str| {
                    for bytes in [
                        "file ".as_bytes(),
                        out_path.as_os_str().as_bytes(),
                        " ".as_bytes(),
                        in_path.as_os_str().as_bytes(),
                        " ".as_bytes(),
                        mode.as_bytes(),
                        "\n".as_bytes(),
                    ] {
                        stdin.deref().write_all(bytes).expect("write");
                    }
                };

                write_dir(Path::new("/bin"));
                write_dir(Path::new("/sbin"));
                write_dir(Path::new("/lib"));
                write_dir(Path::new("/lib/modules"));

                test_distro.iter().for_each(|(name, path)| {
                    if name == "init" {
                        write_file(Path::new("/init"), path, "755 0 0");
                    } else {
                        write_file(&Path::new("/sbin").join(name), path, "755 0 0");
                    }
                });

                // At this point we need to make a slight detour!
                // Preparing the `modules.alias` file inside the VM as part of
                // `/init` is slow. It's faster to prepare it here.
                Command::new("cargo")
                    .arg("run")
                    .args(test_distro_args)
                    .args(["--bin", "depmod", "--", "-b"])
                    .arg(&modules_dir)
                    .status()
                    .context("failed to run depmod")?;

                // Now our modules.alias file is built, we can recursively
                // walk the modules directory and add all the files to the
                // initramfs.
                for entry in WalkDir::new(&modules_dir) {
                    let entry = entry.context("read_dir failed")?;
                    let path = entry.path();
                    let metadata = entry.metadata().context("metadata failed")?;
                    let out_path = Path::new("/lib/modules").join(
                        path.strip_prefix(&modules_dir).with_context(|| {
                            format!(
                                "strip prefix {} failed for {}",
                                path.display(),
                                modules_dir.display()
                            )
                        })?,
                    );
                    if metadata.file_type().is_dir() {
                        write_dir(&out_path);
                    } else if metadata.file_type().is_file() {
                        write_file(&out_path, path, "644 0 0");
                    }
                }

                for (profile, binaries) in binaries {
                    for (name, binary) in binaries {
                        let name = format!("{profile}-{name}");
                        let path = tmp_dir.path().join(&name);
                        copy(&binary, &path).with_context(|| {
                            format!("copy({}, {}) failed", binary.display(), path.display())
                        })?;
                        let out_path = Path::new("/bin").join(&name);
                        write_file(&out_path, &path, "755 0 0");
                    }
                }

                // Must explicitly close to signal EOF.
                drop(stdin);

                let output = gen_init_cpio_child
                    .wait_with_output()
                    .with_context(|| format!("failed to wait for {gen_init_cpio:?}"))?;
                let Output { status, .. } = &output;
                if status.code() != Some(0) {
                    bail!("{gen_init_cpio:?} failed: {output:?}")
                }

                let mut qemu = Command::new(format!("qemu-system-{guest_arch}"));
                if let Some(machine) = machine {
                    qemu.args(["-machine", machine]);
                }
                if let Some(cpu) = cpu {
                    qemu.args(["-cpu", cpu]);
                }
                for accel in ["kvm", "hvf", "tcg"] {
                    qemu.args(["-accel", accel]);
                }
                let console = OsString::from(console);
                let mut kernel_args = std::iter::once(("console", &console))
                    .chain(run_args.clone().map(|run_arg| ("init.arg", run_arg)))
                    .enumerate()
                    .fold(OsString::new(), |mut acc, (i, (k, v))| {
                        if i != 0 {
                            acc.push(" ");
                        }
                        acc.push(k);
                        acc.push("=");
                        acc.push(v);
                        acc
                    });
                // We sometimes see kernel panics containing:
                //
                // [    0.064000] Kernel panic - not syncing: IO-APIC + timer doesn't work!  Boot with apic=debug and send a report.  Then try booting with the 'noapic' option.
                //
                // Heed the advice and boot with noapic. We don't know why this happens.
                kernel_args.push(" noapic");
                qemu.args(["-no-reboot", "-nographic", "-m", "512M", "-smp", "2"])
                    .arg("-append")
                    .arg(kernel_args)
                    .arg("-kernel")
                    .arg(&kernel_image)
                    .arg("-initrd")
                    .arg(&initrd_image);
                let mut qemu_child = qemu
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .with_context(|| format!("failed to spawn {qemu:?}"))?;
                let Child {
                    stdin,
                    stdout,
                    stderr,
                    ..
                } = &mut qemu_child;
                let stdin = stdin.take().unwrap();
                let stdin = Arc::new(Mutex::new(stdin));
                let stdout = stdout.take().unwrap();
                let stdout = BufReader::new(stdout);
                let stderr = stderr.take().unwrap();
                let stderr = BufReader::new(stderr);

                const TERMINATE_AFTER_COUNT: &[(&str, usize)] = &[
                    ("end Kernel panic", 0),
                    ("rcu: RCU grace-period kthread stack dump:", 0),
                    ("watchdog: BUG: soft lockup", 1),
                ];
                let mut counts = [0; TERMINATE_AFTER_COUNT.len()];

                let mut terminate_if_kernel_hang =
                    move |line: &str, stdin: &Arc<Mutex<ChildStdin>>| -> anyhow::Result<()> {
                        if let Some(i) = TERMINATE_AFTER_COUNT
                            .iter()
                            .position(|(marker, _)| line.contains(marker))
                        {
                            counts[i] += 1;

                            let (marker, max) = TERMINATE_AFTER_COUNT[i];
                            if counts[i] > max {
                                println!("{marker} detected > {max} times; terminating QEMU");
                                let mut stdin = stdin.lock().unwrap();
                                stdin
                                    .write_all(&[0x01, b'x'])
                                    .context("failed to write to stdin")?;
                                println!("waiting for QEMU to terminate");
                            }
                        }
                        Ok(())
                    };

                let stderr = {
                    let stdin = stdin.clone();
                    thread::Builder::new()
                        .spawn(move || {
                            for line in stderr.lines() {
                                let line = line.context("failed to read line from stderr")?;
                                eprintln!("{line}");
                                terminate_if_kernel_hang(&line, &stdin)?;
                            }
                            anyhow::Ok(())
                        })
                        .unwrap()
                };

                let mut outcome = None;
                for line in stdout.lines() {
                    let line = line.context("failed to read line from stdout")?;
                    println!("{line}");
                    terminate_if_kernel_hang(&line, &stdin)?;
                    // The init program will print "init: success" or "init: failure" to indicate
                    // the outcome of running the binaries it found in /bin.
                    if let Some(line) = line.strip_prefix("init: ") {
                        let previous = match line {
                            "success" => outcome.replace(Ok(())),
                            "failure" => outcome.replace(Err(())),
                            line => bail!("unexpected init output: {}", line),
                        };
                        if let Some(previous) = previous {
                            bail!("multiple exit status: previous={previous:?}, current={line}");
                        }
                    }
                }

                let output = qemu_child
                    .wait_with_output()
                    .with_context(|| format!("failed to wait for {qemu:?}"))?;
                let Output { status, .. } = &output;
                if status.code() != Some(0) {
                    bail!("{qemu:?} failed: {output:?}")
                }

                stderr.join().unwrap()?;

                let outcome = outcome.ok_or(anyhow!("init did not exit"))?;
                match outcome {
                    Ok(()) => {}
                    Err(()) => {
                        errors.push(anyhow!("VM binaries failed on {}", kernel_image.display()))
                    }
                }
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(Errors::new(errors).into())
            }
        }
    }
}
