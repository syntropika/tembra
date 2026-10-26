plugins {
    id("com.android.library") version "8.9.2"
    id("org.jetbrains.kotlin.android") version "2.1.20"
    id("maven-publish")
}

android {
    namespace = "com.tembra.kernel"
    compileSdk = 36
    defaultConfig {
        minSdk = 24
        consumerProguardFiles("../consumer-rules.pro")
    }
    sourceSets["main"].java.srcDirs("../src/main/kotlin", "../src/generated/kotlin")
    sourceSets["main"].manifest.srcFile("../src/main/AndroidManifest.xml")
    sourceSets["main"].jniLibs.srcDir("../src/main/jniLibs")
    sourceSets["test"].java.srcDir("../src/test/kotlin")
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    publishing { singleVariant("release") { withSourcesJar() } }
    testOptions.unitTests.all {
        val library = providers.gradleProperty("tembraHostLibraryDir").orElse("../../target/debug")
        it.systemProperty("jna.library.path", rootProject.file(library.get()).absolutePath)
    }
}

dependencies {
    compileOnly("androidx.annotation:annotation:1.9.1")
    implementation("net.java.dev.jna:jna:5.17.0@aar")
    testImplementation("junit:junit:4.13.2")
    testRuntimeOnly("net.java.dev.jna:jna:5.17.0@jar")
}

publishing {
    repositories {
        maven {
            name = "package"
            url = uri(rootProject.layout.buildDirectory.dir("repository"))
        }
    }
}

afterEvaluate {
    publishing.publications.create<MavenPublication>("release") {
        from(components["release"])
        artifactId = "tembra-kotlin"
    }
}
