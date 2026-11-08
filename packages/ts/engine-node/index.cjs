const binding = require(`./dist/tembra-${process.platform}-${process.arch}.node`);

// Explicit properties also expose the documented bindings to Node's ESM named-export detection.
exports.NodeKernel = binding.NodeKernel;
exports.compileContract = binding.compileContract;
exports.NodeContract = binding.NodeContract;
exports.NodeInterpreter = binding.NodeInterpreter;
exports.NodeInterpreterPort = binding.NodeInterpreterPort;
exports.NodeComponentPort = binding.NodeComponentPort;
