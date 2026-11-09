#import "RCTTembra.h"
#import "Tembra-Swift.h"

@implementation RCTTembra {
  Tembra *_storage;
}

RCT_EXPORT_MODULE(NativeTembra)

/** Construct the Swift owner without allocating a kernel until requested. */
- (instancetype)init {
  if (self = [super init]) {
    _storage = [Tembra new];
  }
  return self;
}

/** Allow module creation outside the main queue. */
+ (BOOL)requiresMainQueueSetup {
  return NO;
}

/** Allocate one kernel and translate transport errors into JavaScript exceptions. */
- (NSString *)createKernel {
  NSError *error = nil;
  NSString *handle = [_storage createKernelAndReturnError:&error];
  if (error) {
    @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil];
  }
  return handle;
}

/** Preserve the kernel response envelope across the Swift adapter. */
- (NSString *)request:(NSString *)handle metadata:(NSString *)metadata {
  NSError *error = nil;
  NSString *response = [_storage request:handle metadata:metadata error:&error];
  if (error) {
    @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil];
  }
  return response;
}

/** Release storage after JavaScript host cleanup. */
- (NSNumber *)releaseKernel:(NSString *)handle {
  return @([_storage releaseKernel:handle]);
}


/** Forward one host-owned execution operation to the Swift storage. */
- (NSString *)createArtifact:(NSString *)handle bytes:(NSString *)bytes {
  NSError *error = nil;
  NSString *result = [_storage createArtifact:handle bytes:bytes error:&error];
  if (error) { @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil]; }
  return result;
}

/** Forward one host-owned execution operation to the Swift storage. */
- (NSString *)artifactDescription:(NSString *)handle artifact:(NSString *)artifact {
  NSError *error = nil;
  NSString *result = [_storage artifactDescription:handle artifact:artifact error:&error];
  if (error) { @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil]; }
  return result;
}

/** Forward one host-owned execution operation to the Swift storage. */
- (NSString *)artifactRequest:(NSString *)handle artifact:(NSString *)artifact packet:(NSString *)packet {
  NSError *error = nil;
  NSString *result = [_storage artifactRequest:handle artifact:artifact packet:packet error:&error];
  if (error) { @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil]; }
  return result;
}

/** Forward one host-owned execution operation to the Swift storage. */
- (NSNumber *)artifactBind:(NSString *)handle artifact:(NSString *)artifact binding:(NSString *)binding method:(NSString *)method exports:(BOOL)exports {
  NSError *error = nil;
  NSNumber *result = [_storage artifactBind:handle artifact:artifact binding:binding method:method exports:exports error:&error];
  if (error) { @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil]; }
  return result;
}

/** Forward one host-owned execution operation to the Swift storage. */
- (NSString *)artifactValidate:(NSString *)handle artifact:(NSString *)artifact slot:(double)slot arguments_:(BOOL)arguments_ value:(NSString *)value {
  NSError *error = nil;
  NSString *result = [_storage artifactValidate:handle artifact:artifact slot:@(slot) arguments:arguments_ value:value error:&error];
  if (error) { @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil]; }
  return result;
}

/** Forward one host-owned execution operation to the Swift storage. */
- (NSNumber *)releaseArtifact:(NSString *)handle artifact:(NSString *)artifact {
  NSError *error = nil;
  NSNumber *result = [_storage releaseArtifact:handle artifact:artifact error:&error];
  if (error) { @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil]; }
  return result;
}

/** Forward one host-owned execution operation to the Swift storage. */
- (NSString *)portDescription:(NSString *)handle port:(NSString *)port {
  NSError *error = nil;
  NSString *result = [_storage portDescription:handle port:port error:&error];
  if (error) { @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil]; }
  return result;
}

/** Forward one host-owned execution operation to the Swift storage. */
- (NSNumber *)portBegin:(NSString *)handle port:(NSString *)port packet:(NSString *)packet declaration:(BOOL)declaration {
  NSError *error = nil;
  NSNumber *result = [_storage portBegin:handle port:port packet:packet declaration:declaration error:&error];
  if (error) { @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil]; }
  return result;
}

/** Forward one host-owned execution operation to the Swift storage. */
- (NSNumber *)portReply:(NSString *)handle port:(NSString *)port id:(double)id packet:(NSString *)packet {
  NSError *error = nil;
  NSNumber *result = [_storage portReply:handle port:port id:@(id) packet:packet error:&error];
  if (error) { @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil]; }
  return result;
}

/** Forward one host-owned execution operation to the Swift storage. */
- (NSNumber *)portFinish:(NSString *)handle port:(NSString *)port {
  NSError *error = nil;
  NSNumber *result = [_storage portFinish:handle port:port error:&error];
  if (error) { @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil]; }
  return result;
}

/** Forward one host-owned execution operation to the Swift storage. */
- (NSNumber *)portRelease:(NSString *)handle port:(NSString *)port environment:(double)environment {
  NSError *error = nil;
  NSNumber *result = [_storage portRelease:handle port:port environment:@(environment) error:&error];
  if (error) { @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil]; }
  return result;
}

/** Forward one host-owned execution operation to the Swift storage. */
- (NSNumber *)closePort:(NSString *)handle port:(NSString *)port {
  NSError *error = nil;
  NSNumber *result = [_storage closePort:handle port:port error:&error];
  if (error) { @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil]; }
  return result;
}

/** Forward one host-owned execution operation to the Swift storage. */
- (void)configureExecution:(NSString *)handle engine:(NSString *)engine configuration:(NSString *)configuration resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [_storage configureExecution:handle engine:engine configuration:configuration resolve:^(NSString *value) { resolve(value); } reject:^(NSString *message) { reject(@"tembra_execution", message, nil); }];
}

/** Forward one host-owned execution operation to the Swift storage. */
- (void)openPort:(NSString *)handle artifact:(NSString *)artifact bindings:(NSString *)bindings grants:(NSString *)grants component:(BOOL)component resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [_storage openPort:handle artifact:artifact bindings:bindings grants:grants component:component resolve:^(NSString *value) { resolve(value); } reject:^(NSString *message) { reject(@"tembra_execution", message, nil); }];
}

/** Forward one host-owned execution operation to the Swift storage. */
- (void)portReceive:(NSString *)handle port:(NSString *)port resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [_storage portReceive:handle port:port resolve:^(NSString *value) { resolve(value); } reject:^(NSString *message) { reject(@"tembra_execution", message, nil); }];
}

/** Forward one host-owned execution operation to the Swift storage. */
- (void)portIo:(NSString *)handle port:(NSString *)port packet:(NSString *)packet resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [_storage portIo:handle port:port packet:packet resolve:^(NSString *value) { resolve(value); } reject:^(NSString *message) { reject(@"tembra_execution", message, nil); }];
}

/** Forward one host-owned execution operation to the Swift storage. */
- (void)componentCall:(NSString *)handle port:(NSString *)port binding:(NSString *)binding method:(NSString *)method arguments_:(NSString *)arguments_ resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [_storage componentCall:handle port:port binding:binding method:method arguments:arguments_ resolve:^(NSString *value) { resolve(value); } reject:^(NSString *message) { reject(@"tembra_execution", message, nil); }];
}

/** Transfer bounded guest diagnostics without changing execution authority. */
- (NSString *)portLogs:(NSString *)handle port:(NSString *)port {
  NSError *error = nil;
  NSString *result = [_storage portLogs:handle port:port error:&error];
  if (error) { @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil]; }
  return result;
}

/** Resolve an app-owned directory without granting an Orb access to it. */
- (NSString *)filesystemDirectory:(NSString *)kind {
  NSError *error = nil;
  NSString *result = [_storage filesystemDirectory:kind error:&error];
  if (error) { @throw [NSException exceptionWithName:@"TembraError" reason:error.localizedDescription userInfo:nil]; }
  return result;
}

/** Await physical shutdown after an execution transport failure. */
- (void)abortPort:(NSString *)handle port:(NSString *)port resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [_storage abortPort:handle port:port resolve:^(NSString *value) { resolve(value); } reject:^(NSString *message) { reject(@"tembra_execution", message, nil); }];
}

/** Tear down all remaining kernels during React Native invalidation. */
- (void)invalidate {
  [_storage invalidate];
}

/** Connect the generated synchronous methods to JSI. */
- (std::shared_ptr<facebook::react::TurboModule>)getTurboModule:
    (const facebook::react::ObjCTurboModule::InitParams &)params {
  return std::make_shared<facebook::react::NativeTembraSpecJSI>(params);
}
@end
