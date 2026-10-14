# JNA resolves Java peers of native structures and callbacks reflectively. Keep its classes intact.
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
-dontwarn java.awt.**
-dontwarn com.sun.jna.**

# UniFFI generated bindings: JNA maps `UniffiLib` methods to native symbols by name and
# instantiates Structure/Callback subclasses reflectively.
-keep class uniffi.ide_ffi.** { *; }
