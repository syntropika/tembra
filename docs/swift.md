# Swift

`TembraNative` exposes the Rust metadata kernel on iOS and macOS.
Use it from a native application without React Native.
The minimum versions are iOS 15.1 and macOS 11.

## Use the package

Follow [Native builds](../DEVELOP.md#native-builds), then add `packages/swift` as a local Swift package dependency.
Select the `TembraNative` library product. CocoaPods users can select the local `TembraNative.podspec`.

```swift
import TembraNative

let kernel = try TembraKernel()
defer { kernel.close() }
print(try kernel.request("{\"op\":\"features\"}"))
```

`request()` returns the JSON result envelope from the kernel.
Kernel protocol errors remain in that envelope. Transport failures throw Swift errors.
Each kernel owns one Rust worker. Calls from different threads use that same worker.
`close()` stops the worker. Repeated closure is safe; later requests fail.

These bindings expose metadata commands. They do not declare or execute Swift orbs.
The application must complete lifecycle cleanup before it closes the kernel.

See [Architecture](architecture.md) and the [metadata protocol](../crates/bridge/src/protocol.rs).
