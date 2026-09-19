# Reached from Rust over JNI by name, where no shrinker can see the call.
-keep class org.rustls.platformverifier.** { *; }
-keepclassmembers class io.github.overcuriousity.engram.core.contained.Core { native <methods>; }
