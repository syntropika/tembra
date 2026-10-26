module.exports = {
  dependency: {
    platforms: {
      ios: {},
      android: {
        sourceDir: "./android",
        packageImportPath: "import com.tembra.TembraPackage;",
        packageInstance: "new TembraPackage()",
      },
    },
  },
};
