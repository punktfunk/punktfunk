#import "TestLanguage.h"

// Tests assert the source language's text whatever the machine's language: English goes into
// the in-process argument domain when the test bundle loads, before any string is looked up.
@implementation PFTestLanguage
+ (void)load {
    [NSUserDefaults.standardUserDefaults setVolatileDomain:@{@"AppleLanguages" : @[ @"en" ]}
                                                   forName:NSArgumentDomain];
}
@end
