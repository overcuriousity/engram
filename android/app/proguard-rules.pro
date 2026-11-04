# JLaTeXMath builds its atoms by reflection: TeX macros are methods looked up
# by name, and the XML that ships in its assets names classes and constants.
# The AAR carries no consumer rules, so without these R8 renames what the XML
# names and every formula in a release build falls back to its source.
-keep class org.scilab.forge.jlatexmath.** { *; }
-keep class ru.noties.jlatexmath.** { *; }
-dontwarn org.scilab.forge.jlatexmath.**
