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
