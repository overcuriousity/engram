plugins {
    alias(libs.plugins.android.library)
    alias(libs.plugins.kotlin.serialization)
    alias(libs.plugins.ksp)
    alias(libs.plugins.room)
}

android {
    namespace = "io.github.overcuriousity.engram.core"
    compileSdk = 37
    defaultConfig {
        minSdk = 29
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        consumerProguardFiles("consumer-rules.pro")
    }
    // Every launch-time crash this app had was an API newer than minSdk, which
    // the compiler cannot see and only lint does. Fatal, and run in CI.
    lint {
        abortOnError = true
        error += "NewApi"
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    testOptions.unitTests.isReturnDefaultValues = true
    testOptions.unitTests.isIncludeAndroidResources = true
    // `-Pengram.live.origin=… -Pengram.live.token=…` points LiveServerTest at a
    // running engram. Absent, that test skips itself.
    testOptions.unitTests.all { t ->
        listOf("engram.live.origin", "engram.live.token").forEach { k ->
            providers.gradleProperty(k).orNull?.let { t.systemProperty(k, it) }
        }
    }
}


// The Rust core, built for the phone. Only with `-Pengram.native=1`: a
// checkout with no Rust toolchain must still build the app, and then runs in
// server mode only. `cargo +stable` because the Android target lives on the
// rustup toolchain, whatever the machine's everyday cargo is.
val buildNative by tasks.registering(Exec::class) {
    val ndk = providers.environmentVariable("ANDROID_NDK_HOME")
        .orElse(androidComponents.sdkComponents.sdkDirectory.map { it.dir("ndk/28.2.13676358").asFile.absolutePath })
    workingDir = rootProject.file("native")
    environment("ANDROID_NDK_HOME", ndk.get())
    // llama.cpp's build reads this one and not the other, and failing it takes
    // whatever NDK it finds: one library from two NDKs.
    environment("ANDROID_NDK", ndk.get())
    commandLine(
        "cargo", "+stable", "ndk", "-t", "arm64-v8a", "--platform", "29",
        "-o", file("src/main/jniLibs").absolutePath, "build", "--release",
    )
}
if (providers.gradleProperty("engram.native").isPresent) {
    tasks.named("preBuild") { dependsOn(buildNative) }
}

// What the core's certificate verifier calls into over JNI. With the .so or
// not at all; the repository it comes from is in settings.gradle.kts.
if (providers.gradleProperty("engram.native").isPresent) {
    dependencies { implementation("rustls:rustls-platform-verifier:0.1.1") }
}

room { schemaDirectory("$projectDir/schemas") }

dependencies {
    implementation(libs.core.ktx)
    implementation(libs.coroutines.android)
    implementation(libs.serialization.json)
    implementation(libs.okhttp)
    implementation(libs.room.runtime)
    ksp(libs.room.compiler)
    implementation(libs.work.runtime)
    implementation(libs.unifiedpush)

    testImplementation(libs.junit)
    testImplementation(libs.coroutines.test)
    testImplementation(libs.mockwebserver)
    // Room's Android builder wants a Context; Robolectric lends one so the DAO
    // tests stay on the JVM instead of needing an emulator.
    testImplementation(libs.robolectric)
    testImplementation(libs.androidx.test.core)
    androidTestImplementation(libs.androidx.test.core)
    androidTestImplementation(libs.androidx.test.runner)
    androidTestImplementation(libs.androidx.test.junit)
    androidTestImplementation(libs.work.testing)
    androidTestImplementation(libs.mockwebserver)
}
