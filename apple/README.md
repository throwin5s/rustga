# rustga for Apple platforms

SwiftUI host for the shared Rust core. The core is built as a static library and
called through the small C ABI in [`../ffi`](../ffi).

The SwiftUI host supports macOS 26 and iOS 26. The development build has been
installed and launched on a physical iPhone; full device testing is still pending.

## Build and run

```sh
cargo build -p rustga-ffi
swift build --package-path apple
./apple/.build/debug/Rustga
```

The package links `../target/debug/librustga_core.a`, so build the core first.

## iPhone and simulator

Install Xcode and the Rust targets once:

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
open apple/Rustga.xcodeproj
```

Select the Rustga scheme and an iOS simulator or a connected iPhone. The Xcode
build phase builds the matching Rust static library automatically. For a phone,
select your own signing team and a unique bundle identifier in Signing &
Capabilities, then build and run. The included `com.example.rustga` identifier
is a placeholder. The simulator target currently requires an Apple silicon Mac.

The iOS host is experimental: test browsing, downloads, offline reading, and
battery use on the device before relying on it. Some source-specific challenge
handling is macOS-only and may not work on iOS.
