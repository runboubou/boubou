// Exercise real playback cancellation with silence. No input node or microphone is opened.
#define main boubouAudioEntry
#include "audio.m"
#undef main

static void require(AudioHost *host, BOOL condition, const char *message) {
    if (!condition) {
        fprintf(stderr, "Native playback regression: %s (%s)\n", message, host.failure.UTF8String ?: "no native error");
        [host stop];
        exit(1);
    }
}

int main(void) {
    @autoreleasepool {
        AudioHost *host = [AudioHost new];
        NSError *error = nil;
        [host.engine attachNode:host.player];
        [host.engine connect:host.player to:host.engine.mainMixerNode format:host.playFormat];
        require(host, [host.engine startAndReturnError:&error], "output device did not start");
        [host.player play];
        uint64_t epoch = 0;
        NSMutableData *payload = [NSMutableData dataWithBytes:&epoch length:sizeof epoch];
        [payload increaseLengthBy:480 * 2];
        for (int i = 0; i < 400; i++) {
            require(host, [host playback:payload error:&error], "could not schedule silent audio");
        }
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 200 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{
            [host.lock lock];
            NSUInteger played = host.events.count;
            [host.events removeAllObjects];
            [host.lock unlock];
            require(host, played > 0, "positive control: no actual playback acknowledgments");

            uint64_t nextEpoch = CFSwapInt64HostToLittle(1);
            NSError *clearError = nil;
            require(host, [host clear:[NSData dataWithBytes:&nextEpoch length:8] error:&clearError], "could not interrupt playback");
            dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 100 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{
                [host.lock lock];
                NSUInteger obsolete = host.events.count;
                BOOL failed = host.failure != nil;
                [host.lock unlock];
                require(host, !failed && obsolete == 0, "cancelled buffers filled the capture event queue");
                require(host, host.queuedSamples == 0, "cancelled samples remain pending");

                NSMutableData *nextPayload = [payload mutableCopy];
                memcpy(nextPayload.mutableBytes, &nextEpoch, 8);
                NSError *playError = nil;
                for (int i = 0; i < 5; i++) {
                    require(host, [host playback:nextPayload error:&playError], "next playback did not start");
                }
                dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 300 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{
                    [host.lock lock];
                    NSUInteger completed = host.events.count;
                    BOOL failed = host.failure != nil;
                    [host.lock unlock];
                    require(host, !failed && completed == 5 && host.queuedSamples == 0, "next playback did not complete normally");
                    fprintf(stderr, "NATIVE_CANCEL buffers=400 obsolete_acks=0 overflow=0 next_epoch_completed=5 pending_samples=0\n");
                    [host stop];
                    exit(0);
                });
            });
        });
        dispatch_main();
    }
    return 0;
}
