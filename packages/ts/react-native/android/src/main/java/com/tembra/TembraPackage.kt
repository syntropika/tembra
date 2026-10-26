package com.tembra

import com.facebook.react.BaseReactPackage
import com.facebook.react.bridge.NativeModule
import com.facebook.react.bridge.ReactApplicationContext
import com.facebook.react.module.model.ReactModuleInfo
import com.facebook.react.module.model.ReactModuleInfoProvider

/** Register the generated TurboModule with React Native autolinking. */
class TembraPackage : BaseReactPackage() {
    /** Create the module only when React Native requests it. */
    override fun getModule(name: String, reactContext: ReactApplicationContext): NativeModule? =
        if (name == TembraModule.NAME) TembraModule(reactContext) else null

    /** Advertise synchronous TurboModule support to the runtime. */
    override fun getReactModuleInfoProvider() = ReactModuleInfoProvider {
        mapOf(TembraModule.NAME to ReactModuleInfo(
            TembraModule.NAME,
            TembraModule.NAME,
            false,
            false,
            false,
            true,
        ))
    }
}
