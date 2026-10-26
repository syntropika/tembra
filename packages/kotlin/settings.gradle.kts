pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "tembra-kotlin"

include(":jvm")
if (providers.gradleProperty("tembraAndroid").orNull == "true") {
    include(":android")
}
