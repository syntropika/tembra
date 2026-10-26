Pod::Spec.new do |s|
  s.name = 'TembraNative'
  s.version = '0.1.0'
  s.summary = 'Swift bindings for the Tembra metadata kernel'
  s.homepage = 'https://github.com/syntropika/tembra'
  s.license = 'MIT'
  s.authors = 'Tembra contributors'
  s.source = { :git => 'https://github.com/syntropika/tembra.git', :tag => s.version.to_s }
  s.platforms = { :ios => '15.1', :osx => '11.0' }
  s.swift_version = '5.9'
  s.source_files = 'Sources/**/*.swift'
  s.vendored_frameworks = 'native/TembraFFI.xcframework'
  s.frameworks = 'Security', 'SystemConfiguration'
  s.libraries = 'c++', 'resolv'
  s.pod_target_xcconfig = { 'DEFINES_MODULE' => 'YES' }
end
