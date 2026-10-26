require 'json'
package = JSON.parse(File.read(File.join(__dir__, 'package.json')))

Pod::Spec.new do |s|
  s.name = 'Tembra'
  s.version = package['version']
  s.summary = package['description']
  s.homepage = 'https://github.com/syntropika/tembra'
  s.license = package['license']
  s.authors = 'Tembra contributors'
  s.source = { :git => 'https://github.com/syntropika/tembra.git', :tag => s.version.to_s }
  s.platforms = { :ios => '15.1' }
  s.swift_version = '5.9'
  s.source_files = 'ios/**/*.{h,mm,swift}', 'native/generated/swift/*.swift'
  s.public_header_files = 'ios/RCTTembra.h'
  s.vendored_frameworks = 'native/TembraFFI.xcframework'
  s.frameworks = 'Security', 'SystemConfiguration'
  s.libraries = 'c++', 'resolv'
  s.pod_target_xcconfig = { 'DEFINES_MODULE' => 'YES' }
  install_modules_dependencies(s)
end
