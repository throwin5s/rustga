// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "Rustga",
    platforms: [.macOS("26.0")],
    targets: [
        .systemLibrary(name: "RustgaCore", path: "Sources/RustgaCore"),
        .executableTarget(
            name: "Rustga",
            dependencies: ["RustgaCore"],
            path: "Sources/Rustga",
            linkerSettings: [
                // The staticlib is built by `cargo build -p rustga-ffi`.
                .unsafeFlags(["-L../target/debug"]),
                .linkedFramework("Security"),
                .linkedFramework("CoreFoundation"),
                .linkedFramework("SystemConfiguration"),
            ]
        ),
    ]
)
