plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

val ideMinSdk = providers.gradleProperty("ide.minSdk").get().toInt()
val ideCompileSdk = providers.gradleProperty("ide.compileSdk").get().toInt()
val ideNdkVersion = providers.gradleProperty("ide.ndkVersion").get()

/** ABIs shipped in the APK. Must match the ABIs produced by scripts/build-rust-android.sh. */
val shippedAbis = listOf("arm64-v8a", "armeabi-v7a", "x86_64", "x86")

android {
    namespace = "dev.levity.ide"
    compileSdk = ideCompileSdk
    ndkVersion = ideNdkVersion

    defaultConfig {
        applicationId = "dev.levity.ide"
        minSdk = ideMinSdk
        targetSdk = ideCompileSdk
        versionCode = 1
        versionName = "0.1.0"

        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"

        ndk {
            abiFilters += shippedAbis
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
            // CI builds an installable (debug-signed) release APK. Production signing is
            // configured by the release pipeline through a dedicated signing config.
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
    }

    packaging {
        jniLibs {
            // Keep native libraries uncompressed and page aligned so they are mmapped from the APK.
            useLegacyPackaging = false
        }
    }

    testOptions {
        unitTests {
            isReturnDefaultValues = true
        }
    }

    lint {
        warningsAsErrors = false
        abortOnError = true
        checkDependencies = false
        // Generated UniFFI bindings are not hand-maintained code.
        ignoreTestSources = true
    }
}

kotlin {
    jvmToolchain(17)
}

/**
 * Cross-compiles `libide_ffi.so` for every shipped ABI and regenerates the Kotlin bindings.
 * Skip with `-Pide.skipRustBuild=true` when the libraries were already produced by a separate
 * CI step (the build fails fast below if they are missing).
 */
val skipRustBuild = providers.gradleProperty("ide.skipRustBuild").map { it.toBoolean() }.getOrElse(false)
val rustProfile = providers.gradleProperty("ide.rustProfile").getOrElse("release")

val buildRustAndroid by tasks.registering(Exec::class) {
    group = "build"
    description = "Builds libide_ffi.so for all Android ABIs via cargo-ndk and regenerates UniFFI Kotlin bindings."
    workingDir = rootProject.projectDir
    commandLine(
        "bash",
        rootProject.file("scripts/build-rust-android.sh").absolutePath,
        "--$rustProfile",
        "--abis",
        shippedAbis.joinToString(" ")
    )
    onlyIf { !skipRustBuild }
}

val verifyNativeLibraries by tasks.registering {
    group = "verification"
    description = "Fails the build when a shipped ABI is missing libide_ffi.so."
    dependsOn(buildRustAndroid)
    val jniLibsDir = layout.projectDirectory.dir("src/main/jniLibs")
    doLast {
        val missing = shippedAbis.filterNot { jniLibsDir.file("$it/libide_ffi.so").asFile.isFile }
        check(missing.isEmpty()) {
            "Missing libide_ffi.so for ABIs $missing. Run scripts/build-rust-android.sh first."
        }
    }
}

tasks.named("preBuild") {
    dependsOn(verifyNativeLibraries)
}

/**
 * Directory holding the host build of `libide_ffi.so`. `uniffi-bindgen` (run by the Rust build
 * script) compiles the `ide-ffi` crate for the host in the debug profile, which also emits the
 * cdylib. JVM unit tests load it via desktop JNA to exercise the generated bindings end to end.
 */
val hostFfiLibDir: String = providers.gradleProperty("ide.hostLibDir").orElse(
    providers.environmentVariable("CARGO_TARGET_DIR").map { "$it/debug" }
).getOrElse(rootProject.file("../../target/debug").absolutePath)

tasks.withType<Test>().configureEach {
    dependsOn(buildRustAndroid)
    systemProperty("jna.library.path", hostFfiLibDir)
    inputs.file(File(hostFfiLibDir, System.mapLibraryName("ide_ffi"))).withPropertyName("hostFfiLibrary").optional()
}

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2024.10.01")
    implementation(composeBom)
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.7")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.7")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.7")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.ui:ui-text")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0")
    // JNA (Android AAR flavor, ships libjnidispatch.so per ABI) is required by UniFFI Kotlin bindings.
    implementation("net.java.dev.jna:jna:5.14.0@aar")

    debugImplementation("androidx.compose.ui:ui-tooling")

    testImplementation("junit:junit:4.13.2")
    // Desktop JNA jar (ships libjnidispatch for the host) so JVM tests can load the host core.
    testImplementation("net.java.dev.jna:jna:5.14.0")

    androidTestImplementation(composeBom)
    androidTestImplementation("androidx.test.ext:junit:1.2.1")
    androidTestImplementation("androidx.test:runner:1.6.2")
    androidTestImplementation("androidx.compose.ui:ui-test-junit4")
    debugImplementation("androidx.compose.ui:ui-test-manifest")
}
