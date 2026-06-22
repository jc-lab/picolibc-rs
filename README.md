# picolibc

A Cargo crate that **builds [picolibc](https://github.com/picolibc/picolibc) from
source and statically links it into your Rust binary**, for baremetal targets
such as UEFI and `*-unknown-none`.

It exists so you can pull a real C standard library into a `no_std` Rust project
— either to call libc/libm functions from Rust, or to compile and link *other* C
code/libraries (via `cc`, the `cmake` crate, an existing `-sys` crate, …) against
the same libc.

- picolibc is compiled automatically when you depend on this crate; no external
  build steps.
- The resulting `libpicolibc.a` is statically linked into your crate.
- The C **headers** picolibc was built with are exported so downstream crates
  can compile further C against them.
- With the default `bindings` feature, Rust declarations for the libc/libm types,
  functions and constants are generated with [`bindgen`](https://github.com/rust-lang/rust-bindgen).

The picolibc source is vendored as a git submodule at `third_party/picolibc`,
tracking [picolibc-bsd](https://github.com/hyperlight-dev/picolibc-bsd) — a
redistribution of picolibc with all copyleft-licensed files removed (BSD/MIT/
permissive sources only). The build script and source lists are derived from
[hyperlight-libc](https://github.com/hyperlight-dev/hyperlight/tree/main/src/hyperlight_libc)
(Apache-2.0).

## Requirements

- **clang** on the host (picolibc is built freestanding; clang is the compiler
  that can target every baremetal/UEFI triple). cc-rs selects it automatically
  for `*-uefi` targets.
- **libclang** on the host, only when the `bindings` feature is enabled (the
  default). Point `LIBCLANG_PATH` at it if bindgen can't find it, e.g.
  `LIBCLANG_PATH=/usr/lib/llvm-18/lib`.
- The picolibc submodule checked out. `git clone --recursive`, or run
  `git submodule update --init` — the build script also initialises it on demand.

## Usage

Add the dependency and build for a baremetal target:

```toml
[dependencies]
picolibc = "0.1"
```

```console
$ cargo build --target x86_64-unknown-uefi
```

Call libc/libm from Rust through the generated bindings:

```rust
#![no_std]
use core::ffi::c_int;
use picolibc::{snprintf, strlen}; // re-exported from `picolibc::bindings`

unsafe fn demo(buf: *mut u8) {
    let _ = snprintf(buf.cast(), 64, c"hi %d".as_ptr(), 7 as c_int);
}
```

To skip bindgen (static library + C headers only):

```toml
picolibc = { version = "0.1", default-features = false }
```

> **Linking requirement:** a binary only gets picolibc linked if it *uses* this
> crate from Rust (call anything from `picolibc::bindings` or `picolibc::malloc`,
> or add `extern crate picolibc;`). Referencing picolibc's C symbols solely
> through your own `extern "C"` blocks is not enough — the linker won't pull in a
> dependency the Rust code never touches.

### `malloc` feature

picolibc is built without its own allocator, so `malloc`/`free`/`realloc`/`calloc`
must come from somewhere. Enable the `malloc` feature to have this crate export
them (C ABI), forwarding to the Rust global allocator:

```toml
picolibc = { version = "0.1", features = ["malloc"] }
```

The implementation is ported from
[`malloc-rust`](https://github.com/DoumanAsh/malloc-rust) (Boost Software
License 1.0): each allocation stores its size in a header word just before the
returned pointer, and is served by `alloc::alloc`. **Your binary must register a
`#[global_allocator]`** (baremetal/UEFI has no default one); without it the link
fails with "no global memory allocator found". With this feature you no longer
need to hand-write `malloc`/`free`/`realloc`/`calloc` stubs (see below).

### Linking another C library against picolibc

This crate declares `links = "c"`, so for every direct dependent cargo exports
the picolibc include directory as the **`DEP_C_INCLUDE`** environment variable
(and the build output root as `DEP_C_ROOT`) to that dependent's `build.rs`.

A `-sys`-style crate that compiles its own C can therefore build against the same
freestanding libc. With `cc`:

```rust
// build.rs of a crate that depends on `picolibc`
fn main() {
    let include = std::env::var("DEP_C_INCLUDE").unwrap();
    cc::Build::new()
        .compiler("clang")
        .include(&include)
        .flag("-ffreestanding")
        .flag("-nostdlibinc")
        .flag("-fno-builtin")
        .file("vendor/foo.c")
        .compile("foo");
}
```

Or with the [`cmake`](https://crates.io/crates/cmake) crate:

```rust
let include = std::env::var("DEP_C_INCLUDE").unwrap();
let dst = cmake::Config::new("vendor/foo")
    .define("CMAKE_C_FLAGS", format!("-ffreestanding -nostdlibinc -I{include}"))
    .build();
println!("cargo:rustc-link-search=native={}", dst.join("lib").display());
```

> Because cargo allows only one crate per build graph to claim a given `links`
> name, you can have exactly one `links = "c"` crate — which is what you want for
> a baremetal libc. Give your own `-sys` crates a different `links` value.

## Required stubs

picolibc is configured to be freestanding (see [`include/picolibc.h`](include/picolibc.h)):
single-threaded, single global `errno`, tiny stdio, no TLS, no semihosting, and
**no built-in `malloc`**. The embedder must provide a handful of symbols at link
time. Implement them as ordinary Rust functions:

| Symbol | Purpose |
|--------|---------|
| `write` / `read` | back `stdout`/`stderr`/`stdin` for `printf`, `puts`, … |
| `lseek` / `close` | minimal stdio plumbing (may return `-ENOSYS`) |
| `_exit` | terminate; called by `abort` / `exit` |
| `malloc` / `free` / `realloc` / `calloc` | back `strdup`, `asprintf`, `regcomp`, … — **provided for you by the `malloc` feature** |
| `clock_gettime` | only if you use the time APIs |

The `malloc`/`free`/`realloc`/`calloc` row is covered by the [`malloc` feature](#malloc-feature);
enable it instead of writing those by hand. The rest you implement as ordinary
Rust functions:

```rust
use core::ffi::{c_int, c_void};

#[no_mangle]
extern "C" fn write(fd: c_int, buf: *const c_void, count: usize) -> isize {
    if fd != 1 && fd != 2 { return -1; }
    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, count) };
    // ... send `bytes` to your console / UEFI ConOut ...
    let _ = bytes;
    count as isize
}

#[no_mangle]
extern "C" fn _exit(_code: c_int) -> ! {
    // e.g. reset the machine, or spin
    loop {}
}
```

## Target support

All four cells below are build-verified.

| Target family | Status |
|---------------|--------|
| `x86_64-unknown-none`, `x86_64-unknown-linux-*` (ELF / SysV) | Full x86 asm included; `setjmp`/`longjmp` available. |
| `x86_64-unknown-uefi`, `x86_64-pc-windows-*` (COFF / Win64) | mem/str come from C. picolibc's x86 asm (`setjmp`/`longjmp`, interrupt helpers, TLS TCB) is ELF + SysV only and is **not** linked here — provide your own if needed. |
| `aarch64-unknown-none`, `aarch64-unknown-linux-*` (ELF) | Full Neon asm mem/str routines; `setjmp`/`longjmp` available. |
| `aarch64-unknown-uefi`, `aarch64-pc-windows-*` (COFF) | Neon is disabled so picolibc's C fallbacks are used (the asm uses ELF directives); `setjmp`/`longjmp`, interrupt vector and TLS TCB are **not** linked here. |
| `i686-*` | 32-bit x86 sources are present; ELF works, COFF (`i686-unknown-uefi`) is not yet wired up. |
| other arches (riscv, …) | Not yet enumerated — see `arch_config()` in [`build.rs`](build.rs) and the machine source lists under `third_party/picolibc`. |

On aarch64, libm uses picolibc's portable C math (the arch's machine-specific libm
files are pure optimisations and are skipped to keep the build simple).

## Environment overrides

| Variable | Effect |
|----------|--------|
| `PICOLIBC_CC` / `CC` | C compiler to use (default `clang`). |
| `PICOLIBC_CLANG_TARGET` | Force the clang `--target` triple (otherwise cc-rs derives it). |
| `LIBCLANG_PATH` | Where bindgen finds libclang. |

## License

Apache-2.0. Derived works retain the upstream Hyperlight Authors' copyright; the
vendored picolibc sources keep their own permissive licenses (see
`third_party/picolibc`).
