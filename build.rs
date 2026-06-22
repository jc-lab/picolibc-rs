/*
Copyright 2025 The Hyperlight Authors.
Modifications Copyright 2026 The picolibc-rs Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

//! Build script for the `picolibc` crate.
//!
//! It compiles picolibc (vendored under `third_party/picolibc`, a checkout of
//! picolibc-bsd) from source with cc-rs, statically links the resulting archive
//! into the crate, optionally generates Rust bindings with bindgen, and exposes
//! the C include directory to downstream crates.
//!
//! Downstream `-sys` crates that build their own C libraries (with cc-rs, the
//! `cmake` crate, etc.) can read the include directory from the `DEP_C_INCLUDE`
//! environment variable that cargo sets for direct dependents of a crate with
//! `links = "c"`. See the README for usage.

mod build_files;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use anyhow::{bail, Context, Result};
use build_files::{
    LIBC_FILES, LIBC_FILES_AARCH64_ASM, LIBC_FILES_AARCH64_C, LIBC_FILES_AARCH64_EXCLUDE,
    LIBC_FILES_X86, LIBM_FILES, LIBM_FILES_X86,
};

/// Per-architecture build configuration.
struct ArchConfig {
    /// Machine-specific libc sources, relative to `libc/`.
    libc_machine: Vec<&'static str>,
    /// Machine-specific libm sources, relative to `libm/`.
    libm_machine: Vec<&'static str>,
    /// Generic `LIBC_FILES` entries to drop because a machine source replaces
    /// them (avoids duplicate symbols).
    libc_generic_exclude: Vec<&'static str>,
    /// Machine libc sources (relative to `libc/`) that must be dropped on
    /// COFF/Win64 targets such as UEFI, because they are hand-written ELF asm.
    /// Their functionality is then supplied by C implementations, except for a
    /// few symbols that simply become unavailable (`setjmp`/`longjmp`, the
    /// interrupt helpers, the TLS thread-control-block — TLS is disabled here
    /// anyway). No upstream Win64 asm exists for these.
    coff_excluded_libc: Vec<&'static str>,
    /// Arch include sub-directories, relative to the picolibc root.
    include_dirs: Vec<&'static str>,
    /// x86-family target (controls e.g. `-mno-red-zone`).
    is_x86: bool,
    /// aarch64 target (controls the Neon-disabling needed to select the C
    /// fallbacks on COFF targets).
    is_aarch64: bool,
}

/// Resolve the build configuration for the given `CARGO_CFG_TARGET_ARCH`.
fn arch_config(target_arch: &str) -> Result<ArchConfig> {
    match target_arch {
        "x86" | "x86_64" => Ok(ArchConfig {
            libc_machine: LIBC_FILES_X86.to_vec(),
            libm_machine: LIBM_FILES_X86.to_vec(),
            libc_generic_exclude: Vec::new(),
            coff_excluded_libc: vec![
                "machine/x86/interrupt.S",
                "machine/x86/setjmp.S",
                "machine/x86/tcb.S",
            ],
            include_dirs: vec!["libc/machine/x86", "libm/machine/x86"],
            is_x86: true,
            is_aarch64: false,
        }),
        "aarch64" => Ok(ArchConfig {
            // C fallbacks + asm; on ELF the asm wins and the stubs are empty, on
            // COFF (Neon disabled) the stubs provide the implementations.
            libc_machine: LIBC_FILES_AARCH64_C
                .iter()
                .chain(LIBC_FILES_AARCH64_ASM.iter())
                .copied()
                .collect(),
            // The aarch64 libm machine files are pure optimisations of routines
            // that already exist in generic `LIBM_FILES`; use the portable C.
            libm_machine: Vec::new(),
            libc_generic_exclude: LIBC_FILES_AARCH64_EXCLUDE.to_vec(),
            coff_excluded_libc: LIBC_FILES_AARCH64_ASM.to_vec(),
            include_dirs: vec!["libc/machine/aarch64"],
            is_x86: false,
            is_aarch64: true,
        }),
        // To add another architecture, enumerate its machine sources (under
        // `third_party/picolibc/{libc,libm}/machine/<arch>`) into
        // `build_files.rs` and add a branch here.
        arch => bail!(
            "picolibc: unsupported target architecture {arch:?}. \
             Only x86/x86_64 and aarch64 are wired up so far; see arch_config() \
             in build.rs to add another architecture."
        ),
    }
}

/// Recursively copy every `*.h` under `base` into `include_dir`, preserving the
/// directory layout. This is what downstream C consumers actually compile
/// against.
fn copy_includes(include_dir: &Path, base: &Path) -> Result<()> {
    let entries =
        fs::read_dir(base).with_context(|| format!("could not open include dir {base:?}"))?;

    for entry in entries {
        let entry = entry.with_context(|| format!("could not read include dir {base:?}"))?;
        let src = entry.path();
        let dst = include_dir.join(entry.file_name());
        let kind = entry
            .file_type()
            .with_context(|| format!("could not find type of {src:?}"))?;

        if kind.is_dir() {
            fs::create_dir_all(&dst)
                .with_context(|| format!("could not create include dir {dst:?}"))?;
            copy_includes(&dst, &src)?;
        } else if src.extension() == Some(std::ffi::OsStr::new("h")) {
            fs::copy(&src, &dst).with_context(|| format!("could not copy header {src:?}"))?;
        }
    }

    Ok(())
}

/// Construct the cc-rs build with the flags/defines/includes common to every
/// picolibc translation unit.
fn cc_build(
    picolibc_dir: &Path,
    manifest_dir: &Path,
    arch: &ArchConfig,
    coff: bool,
) -> Result<cc::Build> {
    let mut build = cc::Build::new();

    // picolibc is freestanding C; clang is the only compiler that can target
    // every baremetal/UEFI triple we care about, so default to it (cc-rs also
    // selects clang for *-uefi targets) while still honouring CC/PICOLIBC_CC.
    let compiler = env::var("PICOLIBC_CC")
        .or_else(|_| env::var("CC"))
        .unwrap_or_else(|_| "clang".to_string());
    build.compiler(compiler);

    // cc-rs derives the right `--target` for the active cargo target on its own.
    // Allow an explicit override for unusual toolchains.
    if let Ok(t) = env::var("PICOLIBC_CLANG_TARGET") {
        build.flag(format!("--target={t}"));
    }

    build
        .std("c18")
        // Freestanding: no host libc, but keep clang's own resource headers
        // (stddef.h, stdarg.h, stdint.h, ...) which picolibc relies on.
        .flag("-ffreestanding")
        .flag("-nostdlibinc")
        .flag("-fno-builtin")
        .flag("-fno-common")
        .flag("-fno-stack-protector")
        // picolibc provides its own _chk fortified variants; don't let the host
        // toolchain inject its own _FORTIFY_SOURCE level on top.
        .flag("-U_FORTIFY_SOURCE")
        // Quiet the noisiest diagnostics in the vendored sources. We do not turn
        // warnings into errors so a new compiler version can't break the build.
        .flag("-Wno-unused-command-line-argument")
        .flag("-Wno-implicit-int")
        .flag("-Wno-missing-braces")
        .flag("-Wno-return-type")
        .warnings(false);

    // No red zone on x86 baremetal: it is unsafe across interrupt/exception
    // boundaries, and Rust's *-uefi / *-none x86_64 targets disable it too, so
    // matching keeps the ABI consistent across the FFI boundary.
    if arch.is_x86 {
        build.flag_if_supported("-mno-red-zone");
    }

    // On aarch64 COFF/Win64 (UEFI) we cannot assemble the ELF Neon asm, so
    // disable Neon: this makes clang leave `__ARM_NEON` undefined, which flips
    // every picolibc mem*/str* source to its portable C `*-stub.c` fallback.
    // Scalar floating point stays enabled, so libm is unaffected.
    if arch.is_aarch64 && coff {
        build.flag("-march=armv8-a+nosimd");
    }

    build
        .define("_LIBC", None)
        .define("_FILE_OFFSET_BITS", "64")
        .define("DEFINE_MEMALIGN", "1")
        .define("DEFINE_POSIX_MEMALIGN", "1");

    // Include order: our frozen picolibc.h config first, then arch machine
    // headers, then the picolibc tree.
    build.include(manifest_dir.join("include"));
    for dir in &arch.include_dirs {
        build.include(picolibc_dir.join(dir));
    }
    build
        .include(picolibc_dir)
        .include(picolibc_dir.join("libc/stdio"))
        .include(picolibc_dir.join("libc/locale"))
        .include(picolibc_dir.join("libc/include"));

    Ok(build)
}

fn add_libc(build: &mut cc::Build, picolibc_dir: &Path, arch: &ArchConfig, coff: bool) {
    for file in LIBC_FILES.iter().chain(arch.libc_machine.iter()) {
        // Skip generic sources replaced by a machine implementation.
        if arch.libc_generic_exclude.contains(file) {
            continue;
        }
        // Skip ELF-only asm on COFF/Win64 targets.
        if coff && arch.coff_excluded_libc.contains(file) {
            continue;
        }
        build.file(picolibc_dir.join("libc").join(file));
    }
}

fn add_libm(build: &mut cc::Build, picolibc_dir: &Path, arch: &ArchConfig) {
    build.include(picolibc_dir.join("libm/common"));
    for file in LIBM_FILES.iter().chain(arch.libm_machine.iter()) {
        build.file(picolibc_dir.join("libm").join(file));
    }
}

/// Ensure the picolibc submodule is checked out.
fn init_submodule(picolibc_dir: &Path) -> Result<()> {
    if picolibc_dir.join("COPYING.picolibc").exists() {
        return Ok(());
    }
    eprintln!("picolibc: third_party/picolibc is empty, initialising submodule");
    let status = Command::new("git")
        .args(["submodule", "update", "--init", "--depth", "1"])
        .arg(picolibc_dir)
        .status()
        .context("failed to run `git submodule update --init`")?;
    if !status.success() {
        bail!(
            "`git submodule update --init {}` failed; check out the picolibc \
             submodule manually",
            picolibc_dir.display()
        );
    }
    Ok(())
}

#[cfg(feature = "bindings")]
fn generate_bindings(include_dir: &Path, out_dir: &Path) -> Result<()> {
    use bindgen::Formatter::Prettyplease;
    use bindgen::RustEdition::Edition2021;

    let header = |name: &str| include_dir.join(name).to_string_lossy().into_owned();

    let mut builder = bindgen::Builder::default();
    for h in [
        "stdlib.h",
        "stdio.h",
        "string.h",
        "math.h",
        "stdint.h",
        "ctype.h",
        "errno.h",
        "time.h",
        "limits.h",
        "signal.h",
        "setjmp.h",
        "locale.h",
        "wchar.h",
        "wctype.h",
        "fenv.h",
        "inttypes.h",
        "sys/time.h",
    ] {
        builder = builder.header(header(h));
    }

    builder = builder
        .clang_arg(format!("-I{}", include_dir.display()))
        .clang_arg("-nostdlibinc")
        .clang_arg("-ffreestanding")
        .use_core()
        .ctypes_prefix("core::ffi")
        .wrap_unsafe_ops(true)
        .rust_edition(Edition2021)
        .formatter(Prettyplease)
        .derive_copy(true)
        .derive_debug(true)
        .derive_default(true)
        // Eq/Ord/Hash are intentionally NOT derived: several picolibc structs
        // hold function pointers, and deriving comparison traits on those emits
        // `unpredictable_function_pointer_comparisons` warnings for no benefit.
        .generate_comments(true)
        .generate_cstr(true)
        .layout_tests(false);

    // Generate bindings with the same target the C was compiled for so that
    // type sizes (notably `long` under the UEFI/Windows data model) match.
    if let Ok(t) = env::var("PICOLIBC_CLANG_TARGET").or_else(|_| env::var("TARGET")) {
        builder = builder.clang_arg(format!("--target={t}"));
    }

    builder
        .generate()
        .context("unable to generate bindings")?
        .write_to_file(out_dir.join("bindings.rs"))
        .context("couldn't write bindings.rs")?;

    Ok(())
}

fn main() -> Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_files.rs");
    println!("cargo:rerun-if-changed=include/picolibc.h");
    println!("cargo:rerun-if-changed=third_party/picolibc/COPYING.picolibc");
    println!("cargo:rerun-if-env-changed=PICOLIBC_CC");
    println!("cargo:rerun-if-env-changed=PICOLIBC_CLANG_TARGET");

    let out_dir = PathBuf::from(env::var("OUT_DIR").context("OUT_DIR not set")?);
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR not set")?);
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").context("CARGO_CFG_TARGET_ARCH not set")?;
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    // UEFI and Windows produce COFF objects using the Win64 calling convention;
    // picolibc's hand-written x86 asm is ELF + SysV and must be skipped there.
    let coff = matches!(target_os.as_str(), "uefi" | "windows");

    let picolibc_dir = manifest_dir.join("third_party/picolibc");
    init_submodule(&picolibc_dir)?;

    let arch = arch_config(&target_arch)?;

    // Compile picolibc (libc + libm) into a single static archive.
    let mut build = cc_build(&picolibc_dir, &manifest_dir, &arch, coff)?;
    add_libc(&mut build, &picolibc_dir, &arch, coff);
    add_libm(&mut build, &picolibc_dir, &arch);

    // Emits `cargo:rustc-link-lib=static=picolibc` and the `-L native=<out>`
    // search path. The static lib is resolved lazily (on demand) at the final
    // link, so only the libc/libm objects actually referenced are pulled in.
    //
    // Note: a binary that wants picolibc linked must *use* this crate from Rust
    // (e.g. call something from `picolibc::bindings`/`picolibc::malloc`), not
    // only reference C symbols through its own `extern "C"` blocks — otherwise
    // the linker never sees the dependency. See the README.
    build.compile("picolibc");

    // Assemble the public include directory: picolibc's own headers plus our
    // configuration header, into a stable location under OUT_DIR.
    let include_dir = out_dir.join("include");
    fs::create_dir_all(&include_dir)
        .with_context(|| format!("could not create {include_dir:?}"))?;
    copy_includes(&include_dir, &picolibc_dir.join("libc/include"))?;
    copy_includes(&include_dir, &manifest_dir.join("include"))?;

    #[cfg(feature = "bindings")]
    generate_bindings(&include_dir, &out_dir)?;

    // Expose the include directory to dependents. Because of `links = "c"`,
    // cargo turns each `cargo:KEY=VALUE` here into `DEP_C_<KEY>` in the build
    // scripts of crates that depend directly on this one — so `cargo:include`
    // becomes `DEP_C_INCLUDE`, and `cargo:root` becomes `DEP_C_ROOT`.
    let include_str = include_dir
        .to_str()
        .context("include dir path was not valid UTF-8")?;
    println!("cargo:include={include_str}");
    println!("cargo:root={}", out_dir.display());

    Ok(())
}
