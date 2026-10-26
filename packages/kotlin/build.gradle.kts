import groovy.json.JsonSlurper

val packageMetadata = JsonSlurper().parse(file("../ts/react-native/package.json")) as Map<*, *>

allprojects {
    group = "dev.tembra"
    version = packageMetadata["version"] as String
}
