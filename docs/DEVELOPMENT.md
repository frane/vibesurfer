# Development

How to run vibesurfer's test suite per platform.

## Mac (host-native, real WKWebView)

Apple toolchain, `xcode-select --install` for the macOS SDK.

```
cargo test --test m6 -- --test-threads=1
```

`--test-threads=1` is required: every test spawns a real `vs serve`
process backed by `WKWebView`, which is hard-pinned to the Cocoa
main thread. Parallel runs saturate the main-thread queue and
produce flaky timing failures that aren't real engine regressions.

The full M6 cell suite finishes in ~35–45s on Apple silicon. Add
`--nocapture` to see fixture/daemon output during a failure.

## Linux (Docker, real WebKitGTK 6)

The Linux backend uses WebKitGTK 6 + GLib + GTK 4. We don't expect
contributors to install those system-wide; the build + test loop
runs inside a container instead.

### One-time setup

```
docker build -t vs-test-linux -f Dockerfile.linux-test .
docker volume create vs-target-linux
docker volume create vs-cargo-linux
```

The volumes cache cargo's index and target dir between runs so
iteration is fast.

### Run the M6 suite inside the container

```
docker run --rm --privileged \
  -v "$PWD":/work \
  -v vs-target-linux:/work/target-linux \
  -v vs-cargo-linux:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/work/target-linux \
  vs-test-linux
```

The default CMD runs `xvfb-run -a cargo test --test m6 --
--test-threads=1 --nocapture`.

`--privileged` is required: WebKitGTK's bubblewrap sandbox needs
unprivileged user namespaces, which Docker's default seccomp
profile blocks. The container is otherwise isolated; the fixture
server only binds 127.0.0.1 inside it.

`--test-threads=1` is required for the same reason as on macOS:
WebKitGTK is bound to the GLib main context on the thread that
initialized GTK. Parallel runs over the same context flake.

### Interactive shell

```
docker run --rm --privileged -it \
  -v "$PWD":/work \
  -v vs-target-linux:/work/target-linux \
  -v vs-cargo-linux:/usr/local/cargo/registry \
  -e CARGO_TARGET_DIR=/work/target-linux \
  vs-test-linux \
  bash
```

Useful for `cargo build` / single-test iteration. xvfb is set up via
`xvfb-run` per command.

## Windows (CI runner, manual verification)

The WebView2 backend is currently `pending-manual-verification` —
code + tests + CI workflow all exist, but the milestone-closing
verification needs hands on a Windows machine. The CI workflow at
`.github/workflows/m6-windows.yml` (lands in the next M6 transaction)
runs the suite on `windows-latest`; the resulting artefact is the
manual sign-off the maintainer applies after reviewing the run.

`cargo test --test m6 -- --test-threads=1` is the same invocation;
the Win32 message loop has the same single-threaded constraint as
the other two platforms.

## Why `--test-threads=1` is non-negotiable

Real-engine integration tests don't get to choose parallelism. Each
platform has exactly one main-thread/main-loop on which its browser
engine renders, dispatches events, and runs JS. Test parallelism
multiplies that single resource by 47 (one fixture page per cell).
The right number of concurrent engines is one. We tried `#[serial]`
and similar dance moves; they're worse than just running serially.

End-to-end, the suite finishes in well under a minute on every
platform. Sequential is fine.
