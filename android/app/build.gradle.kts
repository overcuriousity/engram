plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.kotlin.serialization)
}

// The version the release workflow stamps in, as `-PengramVersion=YYYY.MMDD.N`
// — the same CalVer the server binaries carry, so a phone and its server can
// be spoken about in one number. Absent for every local build, which is what
// the fallback is for.
val engramVersion = providers.gradleProperty("engramVersion").orNull ?: "0.1.0"

// Android will not install an APK whose `versionCode` is below the installed
// one, and Obtainium offers whatever the newest release holds — so this has to
// climb with the calendar and never repeat. `YYYY.MMDD.N` packs into one int
// with room for 99 releases a day: 2026.917.0 is 26_091_700, and the last year
// this can express, 2099, lands at 99_123_199 — an order of magnitude below
// the 2_100_000_000 ceiling.
fun versionCodeOf(v: String): Int {
    val p = v.split(".")
    val (year, mmdd, n) = listOf(0, 1, 2).map { p.getOrNull(it)?.toIntOrNull() ?: 0 }
    if (year < 2000) return 1
    return (year - 2000) * 1_000_000 + mmdd * 100 + n
}

// The signing key, handed in by the environment and never committed. Its
// absence is not an error here — a local `assembleRelease` still builds, and
// comes out unsigned. The workflow is what refuses to publish one.
val keystorePath = providers.environmentVariable("ENGRAM_KEYSTORE").orNull

android {
    namespace = "io.github.overcuriousity.engram"
    compileSdk = 37
    defaultConfig {
        applicationId = "io.github.overcuriousity.engram"
        minSdk = 34
        targetSdk = 37
        versionCode = versionCodeOf(engramVersion)
        versionName = engramVersion
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }
    signingConfigs {
        if (keystorePath != null) {
            create("release") {
                storeFile = file(keystorePath)
                storePassword = providers.environmentVariable("ENGRAM_KEYSTORE_PASSWORD").orNull
                keyAlias = providers.environmentVariable("ENGRAM_KEY_ALIAS").orNull
                keyPassword = providers.environmentVariable("ENGRAM_KEY_PASSWORD").orNull
            }
        }
    }
    buildTypes {
        release {
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            signingConfig = signingConfigs.findByName("release")
        }
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
    buildFeatures { compose = true }
}


// The connector depends on tink; pin the Android artefact so no other
// dependency drags the JVM one in beside it (duplicate classes otherwise).
configurations.configureEach {
    resolutionStrategy {
        force(libs.tink.android.get().toString())
        dependencySubstitution {
            substitute(module("com.google.crypto.tink:tink")).using(module(libs.tink.android.get().toString()))
        }
    }
}

dependencies {
    implementation(project(":core"))
    implementation(libs.core.ktx)
    implementation(libs.coroutines.android)
    implementation(libs.serialization.json)
    implementation(platform(libs.compose.bom))
    implementation(libs.compose.ui)
    implementation(libs.compose.material3)
    implementation(libs.compose.tooling.preview)
    debugImplementation(libs.compose.tooling)
    implementation(libs.activity.compose)
    implementation(libs.navigation.compose)
    implementation(libs.lifecycle.runtime.compose)
    implementation(libs.work.runtime)
    implementation(libs.unifiedpush)
    implementation(libs.camerax.core)
    implementation(libs.camerax.camera2)
    implementation(libs.camerax.lifecycle)
    implementation(libs.camerax.view)
    implementation(libs.zxing)
    testImplementation(libs.junit)
    androidTestImplementation(libs.androidx.test.core)
    androidTestImplementation(libs.androidx.test.runner)
    androidTestImplementation(libs.androidx.test.junit)
}
