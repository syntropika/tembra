# Kotlin

The Kotlin package exposes the Rust metadata kernel on the JVM and Android.
Both distributions use the same `com.tembra.TembraKernel` API and generated bindings.
Neither requires React Native.

## Select a distribution

| Target                    | Maven dependency                     | Native libraries                             |
| ------------------------- | ------------------------------------ | -------------------------------------------- |
| JVM with Java 17 or newer | `dev.tembra:tembra-kotlin-jvm:0.1.0` | JNA resources for the built host targets.    |
| Android API 24 or newer   | `dev.tembra:tembra-kotlin:0.1.0`     | Shared libraries for the built Android ABIs. |

Add one distribution to your application. Both contain the same Kotlin classes.
Each distribution declares the JNA dependency that its runtime needs.
The JVM distribution does not require the Android SDK.

Follow [Native builds](../DEVELOP.md#native-builds) to create the local Maven repository.
Add `packages/kotlin/build/repository` to your application's Maven repositories.
For a JVM application, add this dependency:

```kotlin
implementation("dev.tembra:tembra-kotlin-jvm:0.1.0")
```

## Use the kernel

```kotlin
import com.tembra.TembraKernel

TembraKernel().use { kernel ->
    println(kernel.request("""{"op":"features"}"""))
}
```

`request()` returns the JSON result envelope from the kernel.
Kernel protocol errors remain in that envelope. Transport failures throw Kotlin exceptions.
Each kernel owns one Rust worker. Calls from different threads use that same worker.
`close()` stops the worker and releases its foreign handle. Later requests fail.

These bindings expose metadata commands. They do not declare or execute Kotlin orbs.
The application must complete lifecycle cleanup before it closes the kernel.

See [Architecture](architecture.md) and the [metadata protocol](../crates/bridge/src/protocol.rs).
