#import <Foundation/Foundation.h>
#import <CoreServices/CoreServices.h>
#include <stdio.h>
#include <string.h>

// Native CFStringRef types avoid treating LaunchServices opaque pointers as JXA objects.
int main(int argc, const char *argv[]) {
  @autoreleasepool {
    if (argc == 2 && strcmp(argv[1], "read") == 0) {
      NSMutableArray *handlers = [NSMutableArray array];
      for (NSString *scheme in @[@"http", @"https"]) {
        CFStringRef copied = LSCopyDefaultHandlerForURLScheme((__bridge CFStringRef)scheme);
        id handler = copied ? CFBridgingRelease(copied) : [NSNull null];
        [handlers addObject:@[scheme, handler]];
      }
      NSError *error = nil;
      NSData *json = [NSJSONSerialization dataWithJSONObject:handlers options:0 error:&error];
      if (!json) { fprintf(stderr, "%s\n", error.localizedDescription.UTF8String); return 1; }
      return fwrite(json.bytes, 1, json.length, stdout) == json.length ? 0 : 1;
    }
    if (argc == 4 && strcmp(argv[1], "set") == 0) {
      NSString *scheme = [NSString stringWithUTF8String:argv[2]];
      NSString *handler = [NSString stringWithUTF8String:argv[3]];
      if (!([scheme isEqualToString:@"http"] || [scheme isEqualToString:@"https"]) || !handler.length) return 2;
      OSStatus status = LSSetDefaultHandlerForURLScheme((__bridge CFStringRef)scheme, (__bridge CFStringRef)handler);
      printf("%d\n", (int)status); return 0;
    }
    return 2;
  }
}
