#!/bin/sh
set -eu
cd /tmp
mkdir -p classes deps
javac -g -d classes /generator/src/example/*.java
jar --create --file input.jar -C classes .
mvn -q -f /generator/pom.xml org.apache.maven.plugins:maven-dependency-plugin:3.8.1:copy-dependencies -DoutputDirectory=/tmp/deps
curl -fL --retry 3 -o r8.jar https://storage.googleapis.com/r8-releases/raw/8.3.37/r8.jar
printf '%s\n' '900dfbc649519969fc5a4c7520d6b7355338e565fa1249874e0190b8d61b1199  r8.jar' | sha256sum -c -
for compiler in proguard r8; do
  for scenario in basic exceptions; do
    if [ "$scenario" = basic ]; then
      case_name=$compiler
      main=example.Main
    else
      case_name=${compiler}_exceptions
      main=example.Exceptions
    fi
    output=/output/$case_name
    mkdir -p "$output"
    cat > rules.pro <<RULES
-keep public class $main { public static void main(java.lang.String[]); }
-keepattributes SourceFile,LineNumberTable
-printmapping $output/mapping.txt
RULES
    if [ "$compiler" = proguard ]; then
      java -cp 'deps/*' proguard.ProGuard @rules.pro -injars input.jar -outjars "$case_name.jar" -libraryjars "$JAVA_HOME/jmods/java.base.jmod(!**.jar;!module-info.class)" -dontwarn -dontnote
      java -cp "$case_name.jar" "$main" 2> "$output/input.txt"
      java -cp 'deps/*' proguard.retrace.ReTrace "$output/mapping.txt" "$output/input.txt" > "$output/expected.txt"
      printf '%s\n' 'ProGuard 7.6.1; official ReTrace oracle; OpenJDK 17' > "$output/generator.txt"
    else
      mkdir -p "$case_name-out"
      java -cp r8.jar com.android.tools.r8.R8 --release --classfile --pg-conf rules.pro --lib "$JAVA_HOME" --output "$case_name-out" input.jar
      java -cp "$case_name-out" "$main" 2> "$output/input.txt"
      # R8 8.3's default pattern misses custom exception names in some headers.
      if [ "$scenario" = exceptions ]; then
        set -- --regex '\s*(?:at %c\.%m\(%s(?::%l)?\)|(?:(?:Caused by|Suppressed): |Exception in thread "[^"]+" )?%c(?::.*)?)'
      else
        set --
      fi
      java -cp r8.jar com.android.tools.r8.retrace.Retrace "$@" "$output/mapping.txt" "$output/input.txt" > "$output/expected.txt"
      sha256sum r8.jar > "$output/compiler.sha256"
      printf '%s\n' 'R8 8.3.37; official Retrace oracle; OpenJDK 17' > "$output/generator.txt"
      if [ "$scenario" = exceptions ]; then
        printf '%s\n' 'Explicit --regex for JVM frames and custom exception headers; see generator script.' >> "$output/generator.txt"
      fi
    fi
    test -s "$output/input.txt"
    test -s "$output/mapping.txt"
    if [ "$scenario" = basic ]; then
      grep -q 'example.Helper.fail' "$output/expected.txt"
    else
      grep -q 'Caused by: example.Exceptions' "$output/expected.txt"
      grep -q 'Suppressed: example.Exceptions' "$output/expected.txt"
    fi
  done
done
