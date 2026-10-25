// vdisplay <name> <serial> <w> <h>: a virtual monitor that exists while this runs.
// The same serial gives the same display UUID, so stopping and starting it
// is a monitor being unplugged and plugged back in. Used by workspace-e2e.py.
// macOS removes it only while the main display is awake; under a sleeping
// monitor it stays connected after this exits, until the monitor wakes.
#import <Foundation/Foundation.h>
#import <CoreGraphics/CoreGraphics.h>
@interface CGVirtualDisplayDescriptor : NSObject
@property(retain) dispatch_queue_t queue; @property(retain) NSString *name;
@property unsigned int maxPixelsWide, maxPixelsHigh, vendorID, productID, serialNum; @property CGSize sizeInMillimeters;
@end
@interface CGVirtualDisplayMode : NSObject
- (instancetype)initWithWidth:(unsigned)w height:(unsigned)h refreshRate:(double)r;
@end
@interface CGVirtualDisplaySettings : NSObject
@property(retain) NSArray *modes; @property unsigned int hiDPI;
@end
@interface CGVirtualDisplay : NSObject
- (instancetype)initWithDescriptor:(CGVirtualDisplayDescriptor *)d;
- (BOOL)applySettings:(CGVirtualDisplaySettings *)s;
@property(readonly) unsigned int displayID;
@end
int main(int argc, char **argv) { @autoreleasepool {
  CGVirtualDisplayDescriptor *d = [CGVirtualDisplayDescriptor new];
  d.queue = dispatch_get_main_queue(); d.name = [NSString stringWithUTF8String:argv[1]];
  unsigned w = atoi(argv[3]), h = atoi(argv[4]);
  d.maxPixelsWide = w; d.maxPixelsHigh = h; d.sizeInMillimeters = CGSizeMake(600, 340);
  d.vendorID = 0x1234; d.productID = 0x5678; d.serialNum = atoi(argv[2]);
  CGVirtualDisplay *disp = [[CGVirtualDisplay alloc] initWithDescriptor:d];
  CGVirtualDisplaySettings *s = [CGVirtualDisplaySettings new];
  s.hiDPI = 0; s.modes = @[[[CGVirtualDisplayMode alloc] initWithWidth:w height:h refreshRate:60]];
  BOOL ok = [disp applySettings:s];
  printf("display %u applied=%d\n", disp.displayID, ok); fflush(stdout);
  // Leave with the test that started this. A runner killed by a timeout never
  // reaches its cleanup, and the display it left kept the next run's display
  // (same serial) from connecting: "the virtual display did not connect",
  // 2026-10-24, three leftovers.
  pid_t parent = getppid();
  dispatch_source_t t = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, dispatch_get_main_queue());
  dispatch_source_set_timer(t, dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC), NSEC_PER_SEC, NSEC_PER_SEC / 10);
  dispatch_source_set_event_handler(t, ^{
    if (getppid() != parent) {
      fprintf(stderr, "vdisplay: verdict=parent_gone pid=%d, removing display %u\n", parent, disp.displayID);
      exit(0);
    }
  });
  dispatch_resume(t);
  dispatch_main();
}}
