# Keep serializable classes (kotlinx.serialization uses reflection-free codegen,
# but we still want companion objects preserved for safety).
-keepattributes *Annotation*, InnerClasses
-dontnote kotlinx.serialization.AnnotationsKt
