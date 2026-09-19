pluginManagement {
    repositories { google(); mavenCentral(); gradlePluginPortal() }
}
dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google(); mavenCentral()
        // The Kotlin half of rustls-platform-verifier ships inside its crate,
        // and Cargo is who knows where that is. Only where there is a Cargo:
        // the same switch as the .so, without which those classes would have
        // nothing to be called by.
        if (providers.gradleProperty("engram.native").isPresent) {
            val metadata = providers.exec {
                workingDir = file("native")
                commandLine("cargo", "+stable", "metadata", "--format-version", "1", "--filter-platform", "aarch64-linux-android")
            }.standardOutput.asText.get()
            @Suppress("UNCHECKED_CAST")
            val packages = (groovy.json.JsonSlurper().parseText(metadata) as Map<String, Any>)["packages"] as List<Map<String, Any>>
            val crate = File(packages.first { it["name"] == "rustls-platform-verifier-android" }["manifest_path"] as String).parentFile
            maven { url = uri(File(crate, "maven")); metadataSources.artifact() }
        }
    }
}
rootProject.name = "engram"
include(":core", ":app")
