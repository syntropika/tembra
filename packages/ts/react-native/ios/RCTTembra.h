#import <Foundation/Foundation.h>

// Swift imports the pod's public headers without C++ interoperability enabled.
#ifdef __cplusplus
#import <TembraSpec/TembraSpec.h>
#import <React/RCTInvalidating.h>

@interface RCTTembra : NSObject <NativeTembraSpec, RCTInvalidating>
@end
#endif
