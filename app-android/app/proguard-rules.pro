# R8 keep rules for the RemoteControl app.
#
# Compose / OkHttp / MLKit / CameraX all ship their own consumer-proguard
# rules, so we only need to cover what's specific to this project —
# kotlinx.serialization in particular needs its synthetic $$serializer
# inner classes preserved on every @Serializable type.

# kotlinx.serialization uses compile-time codegen, NOT reflection — but
# the generated $$serializer is referenced from the synthetic Companion
# entry point, which R8 cannot reach without help.
-keepattributes RuntimeVisibleAnnotations,AnnotationDefault

-keep,includedescriptorclasses class com.remotecontrol.app.**$$serializer { *; }
-keepclassmembers class com.remotecontrol.app.** {
    *** Companion;
}
-keepclasseswithmembers class com.remotecontrol.app.** {
    kotlinx.serialization.KSerializer serializer(...);
}
