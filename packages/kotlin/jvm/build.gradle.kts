plugins {
    id("org.jetbrains.kotlin.jvm") version "2.1.20"
    id("maven-publish")
}

kotlin {
    jvmToolchain(17)
    sourceSets["main"].kotlin.srcDirs("../src/main/kotlin", "../src/generated/kotlin")
    sourceSets["test"].kotlin.srcDir("../src/test/kotlin")
}

sourceSets["main"].resources.srcDir("../src/generated/resources")
java { withSourcesJar() }

dependencies {
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.2")
    implementation("net.java.dev.jna:jna:5.17.0")
    testImplementation("junit:junit:4.13.2")
}

publishing {
    publications {
        create<MavenPublication>("release") {
            from(components["java"])
            artifactId = "tembra-kotlin-jvm"
        }
    }
    repositories {
        maven {
            name = "package"
            url = uri(rootProject.layout.buildDirectory.dir("repository"))
        }
    }
}

// Run source-checkout tests against their explicitly selected native build.
tasks.test {
    val library = providers.gradleProperty("tembraHostLibraryDir").orElse("../../target/debug")
    systemProperty("jna.library.path", rootProject.file(library.get()).absolutePath)
}
