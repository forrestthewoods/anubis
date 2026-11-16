#pragma once

#import <Foundation/Foundation.h>

#ifdef __cplusplus
extern "C" {
#endif

// Joins `names` with ", " via std::string (Objective-C++), returns an NSString.
NSString* greeting_join(NSArray<NSString*>* names);

#ifdef __cplusplus
}
#endif
