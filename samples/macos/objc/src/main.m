#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

#include "greeting.h"

int main(void) {
    @autoreleasepool {
        NSArray<NSString*>* names = @[ @"Linux", @"Windows", @"macOS" ];
        NSString* joined = greeting_join(names);
        printf("Hello from Objective-C: %s\n", joined.UTF8String);
        if (![joined isEqualToString:@"Linux, Windows, macOS"]) {
            return 1;
        }

        NSOperatingSystemVersion v = NSProcessInfo.processInfo.operatingSystemVersion;
        printf("macOS %ld.%ld.%ld\n", (long)v.majorVersion, (long)v.minorVersion, (long)v.patchVersion);

        // @available calls __isPlatformVersionAtLeast (darwin_builtins).
        if (@available(macOS 14.0, *)) {
            printf("@available(macOS 14.0): yes\n");
        } else {
            printf("@available(macOS 14.0): no\n");
        }

        // Inside ObjcSample.app, NSBundle finds the bundled resource.
        NSString* res = [NSBundle.mainBundle pathForResource:@"hello" ofType:@"txt" inDirectory:@"resources"];
        if (res) {
            NSString* text = [NSString stringWithContentsOfFile:res encoding:NSUTF8StringEncoding error:nil];
            printf("Bundle %s resource: %s", NSBundle.mainBundle.bundleIdentifier.UTF8String, text.UTF8String);
        } else {
            printf("Not running from an app bundle\n");
        }

        // CI runners are VMs and may have no GPU; just report.
        id<MTLDevice> device = MTLCreateSystemDefaultDevice();
        printf("Metal device: %s\n", device ? device.name.UTF8String : "(none)");
    }
    return 0;
}
