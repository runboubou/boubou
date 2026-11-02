#import <AVFoundation/AVFoundation.h>
#import <CoreAudio/CoreAudio.h>
#import <Foundation/Foundation.h>
#include <errno.h>
#include <mach/mach_time.h>
#include <math.h>
#include <signal.h>
#include <stdatomic.h>
#include <unistd.h>

// Private pipe protocol: byte kind, UInt32 LE payload length, then payload.
// Commands: 1 playback (UInt64 epoch + PCM16/24k), 2 clear (epoch), 3 quit.
// Events: 1 ready, 2 capture (UInt64 host ticks + byte near-end speech + PCM16/16k),
// 3 played (epoch + count), 4 error, 5 clear applied (epoch).
static NSData *packet(uint8_t kind, NSData *payload) {
    NSMutableData *data = [NSMutableData dataWithBytes:&kind length:1];
    uint32_t length = CFSwapInt32HostToLittle((uint32_t)payload.length);
    [data appendBytes:&length length:4];
    if (payload) [data appendData:payload];
    return data;
}

static BOOL writeData(NSData *data) {
    const uint8_t *bytes = data.bytes;
    size_t offset = 0;
    while (offset < data.length) {
        ssize_t written = write(STDOUT_FILENO, bytes + offset, data.length - offset);
        if (written < 0 && errno == EINTR) continue;
        if (written <= 0) return NO;
        offset += (size_t)written;
    }
    return YES;
}

static NSData *readExactly(size_t count, BOOL *eof, NSString **error) {
    NSMutableData *data = [NSMutableData dataWithLength:count];
    size_t offset = 0;
    while (offset < count) {
        ssize_t received = read(STDIN_FILENO, (uint8_t *)data.mutableBytes + offset, count - offset);
        if (received < 0 && errno == EINTR) continue;
        if (received <= 0) {
            *eof = offset == 0 && received == 0;
            if (!*eof) *error = @"Audio command ended mid-packet";
            return nil;
        }
        offset += (size_t)received;
    }
    return data;
}

static uint64_t readU64(NSData *data, size_t offset) {
    uint64_t value;
    memcpy(&value, (const uint8_t *)data.bytes + offset, sizeof value);
    return CFSwapInt64LittleToHost(value);
}

static NSError *audioError(NSString *description) {
    return [NSError errorWithDomain:@"BoubouAudio" code:1 userInfo:@{NSLocalizedDescriptionKey: description}];
}

@interface AudioHost : NSObject {
    int16_t _capture[320];
    size_t _captured;
    uint64_t _captureHostTime;
    uint8_t _captureNearEnd;
    _Atomic(uint32_t) _nearEndSpeech;
    double _hostTicksPerSecond;
    AudioDeviceID _voiceDevice;
    UInt32 _originalVoiceDetection;
    BOOL _voiceDetectionChanged;
    BOOL _voiceListenerRegistered;
    BOOL _tapInstalled;
    BOOL _stopped;
    dispatch_queue_t _voiceQueue;
    AudioObjectPropertyListenerBlock _voiceListener;
}
@property (strong) AVAudioEngine *engine;
@property (strong) AVAudioPlayerNode *player;
@property (strong) AVAudioFormat *playFormat;
@property (strong) NSLock *lock;
@property (strong) NSMutableArray<NSData *> *events;
@property (copy) NSString *failure;
@property (strong) dispatch_source_t writer;
@property (strong) id observer;
@property uint64_t epoch;
@property NSUInteger queuedSamples;
@property BOOL manualRecording;
- (BOOL)start:(NSError **)error;
- (BOOL)startVoiceDetection:(NSError **)error;
- (void)updateVoiceDetection;
- (void)startWriter;
- (void)fail:(NSString *)message;
- (void)finishWithStatus:(int)status;
- (BOOL)playback:(NSData *)payload error:(NSError **)error;
- (BOOL)clear:(NSData *)payload error:(NSError **)error;
- (void)stop;
@end

@implementation AudioHost
- (instancetype)init {
    self = [super init];
    if (self) {
        _engine = [AVAudioEngine new];
        _player = [AVAudioPlayerNode new];
        _lock = [NSLock new];
        _events = [NSMutableArray new];
        _playFormat = [[AVAudioFormat alloc] initWithCommonFormat:AVAudioPCMFormatFloat32 sampleRate:24000 channels:1 interleaved:NO];
        mach_timebase_info_data_t timebase;
        mach_timebase_info(&timebase);
        _hostTicksPerSecond = 1e9 * timebase.denom / timebase.numer;
        atomic_init(&_nearEndSpeech, 0);
    }
    return self;
}

- (void)fail:(NSString *)message {
    [self.lock lock];
    if (!self.failure) self.failure = message;
    [self.lock unlock];
}

- (void)emit:(uint8_t)kind payload:(NSData *)payload {
    [self.lock lock];
    if (self.events.count < 128) {
        [self.events addObject:packet(kind, payload)];
    } else if (!self.failure) {
        self.failure = @"Audio event delivery fell behind. Please restart Boubou.";
    }
    [self.lock unlock];
}

- (void)updateVoiceDetection {
    AudioObjectPropertyAddress stateAddress = {
        kAudioDevicePropertyVoiceActivityDetectionState,
        kAudioDevicePropertyScopeInput,
        kAudioObjectPropertyElementMain
    };
    UInt32 state = 0;
    UInt32 size = sizeof state;
    OSStatus status = AudioObjectGetPropertyData(_voiceDevice, &stateAddress, 0, NULL, &size, &state);
    if (status != noErr || size != sizeof state) {
        atomic_store_explicit(&_nearEndSpeech, 0, memory_order_relaxed);
        [self fail:[NSString stringWithFormat:@"Could not read echo-aware voice activity (CoreAudio %d).", (int)status]];
        return;
    }
    atomic_store_explicit(&_nearEndSpeech, state != 0, memory_order_relaxed);
}

- (BOOL)startVoiceDetection:(NSError **)error {
    AudioObjectPropertyAddress deviceAddress = {
        kAudioHardwarePropertyDefaultInputDevice,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain
    };
    UInt32 size = sizeof _voiceDevice;
    OSStatus status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &deviceAddress, 0, NULL, &size, &_voiceDevice);
    if (status != noErr || _voiceDevice == kAudioObjectUnknown) {
        *error = audioError([NSString stringWithFormat:@"Could not identify the microphone for echo-aware voice detection (CoreAudio %d).", (int)status]);
        return NO;
    }
    AudioObjectPropertyAddress enableAddress = {
        kAudioDevicePropertyVoiceActivityDetectionEnable,
        kAudioDevicePropertyScopeInput,
        kAudioObjectPropertyElementMain
    };
    AudioObjectPropertyAddress stateAddress = {
        kAudioDevicePropertyVoiceActivityDetectionState,
        kAudioDevicePropertyScopeInput,
        kAudioObjectPropertyElementMain
    };
    if (!AudioObjectHasProperty(_voiceDevice, &enableAddress) || !AudioObjectHasProperty(_voiceDevice, &stateAddress)) {
        *error = audioError(@"This microphone does not support macOS echo-aware voice detection. Boubou cannot safely enable voice interruption on this device.");
        return NO;
    }
    size = sizeof _originalVoiceDetection;
    status = AudioObjectGetPropertyData(_voiceDevice, &enableAddress, 0, NULL, &size, &_originalVoiceDetection);
    if (status != noErr || size != sizeof _originalVoiceDetection) {
        *error = audioError([NSString stringWithFormat:@"Could not read the microphone's voice detection setting (CoreAudio %d).", (int)status]);
        return NO;
    }
    _voiceQueue = dispatch_queue_create("ai.entropy.boubou.audio.voice-activity", DISPATCH_QUEUE_SERIAL);
    __weak AudioHost *weakSelf = self;
    _voiceListener = ^(UInt32 count, const AudioObjectPropertyAddress *addresses) {
        [weakSelf updateVoiceDetection];
    };
    status = AudioObjectAddPropertyListenerBlock(_voiceDevice, &stateAddress, _voiceQueue, _voiceListener);
    if (status != noErr) {
        *error = audioError([NSString stringWithFormat:@"Could not listen for echo-aware voice activity (CoreAudio %d).", (int)status]);
        return NO;
    }
    _voiceListenerRegistered = YES;
    if (_originalVoiceDetection == 0) {
        UInt32 enabled = 1;
        status = AudioObjectSetPropertyData(_voiceDevice, &enableAddress, 0, NULL, sizeof enabled, &enabled);
        if (status != noErr) {
            *error = audioError([NSString stringWithFormat:@"Could not enable echo-aware voice activity (CoreAudio %d).", (int)status]);
            return NO;
        }
        _voiceDetectionChanged = YES;
    }
    dispatch_sync(_voiceQueue, ^{ [self updateVoiceDetection]; });
    if (self.failure) {
        *error = audioError(self.failure);
        return NO;
    }
    return YES;
}

- (BOOL)start:(NSError **)error {
    AVAudioInputNode *input = self.engine.inputNode;
    NSError *voiceError = nil;
    if (![input setVoiceProcessingEnabled:YES error:&voiceError]) {
        *error = audioError([NSString stringWithFormat:@"Could not enable echo cancellation: %@", voiceError.localizedDescription]);
        return NO;
    }
    input.voiceProcessingBypassed = NO;
    AVAudioFormat *inputFormat = [input outputFormatForBus:0];
    if (inputFormat.sampleRate <= 0 || inputFormat.channelCount == 0 || [self.engine.outputNode inputFormatForBus:0].sampleRate <= 0) {
        *error = audioError(@"No working microphone or speaker. Connect an audio device and retry.");
        return NO;
    }
    AVAudioFormat *target = [[AVAudioFormat alloc] initWithCommonFormat:AVAudioPCMFormatFloat32 sampleRate:16000 channels:1 interleaved:NO];
    AVAudioConverter *converter = [[AVAudioConverter alloc] initFromFormat:inputFormat toFormat:target];
    if (!converter) {
        *error = audioError(@"Cannot convert this microphone's audio to 16 kHz.");
        return NO;
    }
    converter.channelMap = @[@0];
    [self.engine attachNode:self.player];
    [self.engine connect:self.player to:self.engine.mainMixerNode format:self.playFormat];
    // VoiceProcessingIO requires matching client-side input/output formats.
    // Let the mixer convert model speech (24 kHz) to the negotiated duplex format;
    // otherwise the implicit mixer connection adopts 24 kHz and fails with -10875.
    [self.engine connect:self.engine.mainMixerNode to:self.engine.outputNode format:inputFormat];
    if (!self.manualRecording && ![self startVoiceDetection:error]) return NO;
    __weak AudioHost *weakSelf = self;
    // nil lets the voice-processing node negotiate its own hardware format.
    [input installTapOnBus:0 bufferSize:1024 format:nil block:^(AVAudioPCMBuffer *buffer, AVAudioTime *when) {
        @autoreleasepool {
            AudioHost *host = weakSelf;
            if (!host) return;
            if (!when.isHostTimeValid) {
                [host fail:@"The microphone did not provide a capture timestamp."];
                return;
            }
            AVAudioFrameCount capacity = (AVAudioFrameCount)ceil(buffer.frameLength * 16000.0 / inputFormat.sampleRate) + 32;
            AVAudioPCMBuffer *converted = [[AVAudioPCMBuffer alloc] initWithPCMFormat:target frameCapacity:capacity];
            if (!converted) { [host fail:@"Could not allocate the microphone conversion buffer."]; return; }
            __block BOOL supplied = NO;
            NSError *conversionError = nil;
            AVAudioConverterOutputStatus status = [converter convertToBuffer:converted error:&conversionError withInputFromBlock:^AVAudioBuffer *(AVAudioPacketCount requested, AVAudioConverterInputStatus *state) {
                if (supplied) { *state = AVAudioConverterInputStatus_NoDataNow; return nil; }
                supplied = YES;
                *state = AVAudioConverterInputStatus_HaveData;
                return buffer;
            }];
            if (status == AVAudioConverterOutputStatus_Error || !converted.floatChannelData) {
                [host fail:[NSString stringWithFormat:@"Microphone conversion failed: %@", conversionError.localizedDescription ?: @"unknown error"]];
                return;
            }
            float *samples = converted.floatChannelData[0];
            uint8_t nearEnd = atomic_load_explicit(&host->_nearEndSpeech, memory_order_relaxed) != 0;
            for (AVAudioFrameCount index = 0; index < converted.frameLength; index++) {
                if (host->_captured == 0) {
                    host->_captureHostTime = when.hostTime + (uint64_t)llround(index * host->_hostTicksPerSecond / 16000.0);
                    host->_captureNearEnd = nearEnd;
                } else {
                    host->_captureNearEnd |= nearEnd;
                }
                float value = isfinite(samples[index]) ? samples[index] : 0;
                int16_t pcm = (int16_t)fmaxf(-32768, fminf(32767, roundf(value * 32768)));
                host->_capture[host->_captured++] = CFSwapInt16HostToLittle(pcm);
                if (host->_captured == 320) {
                    uint64_t timestamp = CFSwapInt64HostToLittle(host->_captureHostTime);
                    NSMutableData *frame = [NSMutableData dataWithCapacity:8 + 1 + sizeof host->_capture];
                    [frame appendBytes:&timestamp length:sizeof timestamp];
                    [frame appendBytes:&host->_captureNearEnd length:1];
                    [frame appendBytes:host->_capture length:sizeof host->_capture];
                    [host emit:2 payload:frame];
                    host->_captured = 0;
                }
            }
        }
    }];
    _tapInstalled = YES;
    [self.engine prepare];
    NSError *startError = nil;
    if (![self.engine startAndReturnError:&startError]) {
        *error = audioError([NSString stringWithFormat:@"Could not start microphone and speaker (input %@, output %@): %@", inputFormat, [self.engine.outputNode inputFormatForBus:0], startError.localizedDescription]);
        return NO;
    }
    [self.player play];
    self.observer = [[NSNotificationCenter defaultCenter] addObserverForName:AVAudioEngineConfigurationChangeNotification object:self.engine queue:nil usingBlock:^(NSNotification *notification) {
        [weakSelf fail:@"The audio device changed. Restart Boubou to use the new microphone and speaker."];
    }];
    return YES;
}

- (void)startWriter {
    dispatch_queue_t queue = dispatch_queue_create("ai.entropy.boubou.audio.pipe", DISPATCH_QUEUE_SERIAL);
    self.writer = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, queue);
    dispatch_source_set_timer(self.writer, DISPATCH_TIME_NOW, 10 * NSEC_PER_MSEC, NSEC_PER_MSEC);
    __weak AudioHost *weakSelf = self;
    dispatch_source_set_event_handler(self.writer, ^{
        @autoreleasepool {
            AudioHost *host = weakSelf;
            if (!host) return;
            [host.lock lock];
            NSArray<NSData *> *batch = [host.events copy];
            [host.events removeAllObjects];
            NSString *failure = host.failure;
            [host.lock unlock];
            if (failure) {
                writeData(packet(4, [failure dataUsingEncoding:NSUTF8StringEncoding]));
                [host finishWithStatus:1];
                return;
            }
            for (NSData *data in batch) {
                if (!writeData(data)) { [host finishWithStatus:1]; return; }
            }
        }
    });
    dispatch_resume(self.writer);
}

- (void)finishWithStatus:(int)status {
    dispatch_async(dispatch_get_main_queue(), ^{ [self stop]; exit(status); });
}

- (BOOL)playback:(NSData *)payload error:(NSError **)error {
    if (payload.length < 8 || (payload.length - 8) % 2 != 0) {
        *error = audioError(@"Malformed playback packet");
        return NO;
    }
    uint64_t incomingEpoch = readU64(payload, 0);
    if (incomingEpoch != self.epoch) return YES;
    NSUInteger count = (payload.length - 8) / 2;
    if (count == 0) return YES;
    if (self.queuedSamples + count > 24000 * 30) {
        *error = audioError(@"Playback exceeded the 30-second queue limit");
        return NO;
    }
    AVAudioPCMBuffer *buffer = [[AVAudioPCMBuffer alloc] initWithPCMFormat:self.playFormat frameCapacity:(AVAudioFrameCount)count];
    if (!buffer || !buffer.floatChannelData) {
        *error = audioError(@"Could not allocate a playback buffer");
        return NO;
    }
    buffer.frameLength = (AVAudioFrameCount)count;
    const uint8_t *bytes = payload.bytes;
    for (NSUInteger index = 0; index < count; index++) {
        uint16_t value;
        memcpy(&value, bytes + 8 + index * 2, 2);
        buffer.floatChannelData[0][index] = (int16_t)CFSwapInt16LittleToHost(value) / 32768.0f;
    }
    self.queuedSamples += count;
    __weak AudioHost *weakSelf = self;
    [self.player scheduleBuffer:buffer completionCallbackType:AVAudioPlayerNodeCompletionDataPlayedBack completionHandler:^(AVAudioPlayerNodeCompletionCallbackType type) {
        AudioHost *host = weakSelf;
        if (!host) return;
        dispatch_async(dispatch_get_main_queue(), ^{
            // stop() completes every cancelled buffer. Filter on the same queue as clear()
            // before these obsolete acknowledgments can fill the capture event queue.
            if (incomingEpoch != host.epoch) return;
            host.queuedSamples -= MIN(host.queuedSamples, count);
            uint64_t acknowledgment[] = {CFSwapInt64HostToLittle(incomingEpoch), CFSwapInt64HostToLittle(count)};
            [host emit:3 payload:[NSData dataWithBytes:acknowledgment length:sizeof acknowledgment]];
        });
    }];
    return YES;
}

- (BOOL)clear:(NSData *)payload error:(NSError **)error {
    if (payload.length != 8) { *error = audioError(@"Malformed clear packet"); return NO; }
    self.epoch = readU64(payload, 0);
    // stop discards scheduled audio and the currently rendered buffer.
    [self.player stop];
    self.queuedSamples = 0;
    [self.player play];
    [self emit:5 payload:payload];
    return YES;
}

- (void)stop {
    if (_stopped) return;
    _stopped = YES;
    if (self.observer) {
        [[NSNotificationCenter defaultCenter] removeObserver:self.observer];
        self.observer = nil;
    }
    AudioObjectPropertyAddress stateAddress = {
        kAudioDevicePropertyVoiceActivityDetectionState,
        kAudioDevicePropertyScopeInput,
        kAudioObjectPropertyElementMain
    };
    if (_voiceListenerRegistered) {
        AudioObjectRemovePropertyListenerBlock(_voiceDevice, &stateAddress, _voiceQueue, _voiceListener);
        _voiceListenerRegistered = NO;
        dispatch_sync(_voiceQueue, ^{});
    }
    if (_voiceDetectionChanged) {
        AudioObjectPropertyAddress enableAddress = {
            kAudioDevicePropertyVoiceActivityDetectionEnable,
            kAudioDevicePropertyScopeInput,
            kAudioObjectPropertyElementMain
        };
        OSStatus status = AudioObjectSetPropertyData(_voiceDevice, &enableAddress, 0, NULL, sizeof _originalVoiceDetection, &_originalVoiceDetection);
        if (status != noErr) {
            fprintf(stderr, "Boubou could not restore the microphone voice detection setting (CoreAudio %d).\n", (int)status);
        }
        _voiceDetectionChanged = NO;
    }
    atomic_store_explicit(&_nearEndSpeech, 0, memory_order_relaxed);
    [self.player stop];
    [self.engine stop];
    if (_tapInstalled) {
        [self.engine.inputNode removeTapOnBus:0];
        _tapInstalled = NO;
    }
    if (self.writer) dispatch_source_cancel(self.writer);
}
@end

static BOOL microphonePermission(void) {
    AVAuthorizationStatus status = [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio];
    if (status == AVAuthorizationStatusAuthorized) return YES;
    if (status != AVAuthorizationStatusNotDetermined) return NO;
    dispatch_semaphore_t finished = dispatch_semaphore_create(0);
    __block BOOL allowed = NO;
    [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio completionHandler:^(BOOL granted) {
        allowed = granted;
        dispatch_semaphore_signal(finished);
    }];
    dispatch_semaphore_wait(finished, DISPATCH_TIME_FOREVER);
    return allowed;
}

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        signal(SIGPIPE, SIG_IGN);
        if (!microphonePermission()) {
            NSString *message = @"Microphone access was denied. Enable Boubou or your terminal in System Settings > Privacy & Security > Microphone, then retry.";
            writeData(packet(4, [message dataUsingEncoding:NSUTF8StringEncoding]));
            return 1;
        }
        AudioHost *host = [AudioHost new];
        host.manualRecording = argc == 2 && strcmp(argv[1], "--manual") == 0;
        NSError *startupError = nil;
        if (![host start:&startupError]) {
            [host stop];
            writeData(packet(4, [startupError.localizedDescription dataUsingEncoding:NSUTF8StringEncoding]));
            return 1;
        }
        if (!writeData(packet(1, nil))) { [host stop]; return 1; }
        [host startWriter];
        dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
            @autoreleasepool {
                while (YES) {
                    BOOL eof = NO;
                    NSString *failure = nil;
                    NSData *header = readExactly(5, &eof, &failure);
                    if (!header) {
                        if (failure) { [host fail:failure]; return; }
                        break;
                    }
                    uint32_t length;
                    memcpy(&length, (const uint8_t *)header.bytes + 1, 4);
                    length = CFSwapInt32LittleToHost(length);
                    if (length > 2000000) { [host fail:@"Audio command is too large"]; return; }
                    NSData *payload = readExactly(length, &eof, &failure);
                    if (!payload) { [host fail:failure ?: @"Audio command has no payload"]; return; }
                    uint8_t kind = ((const uint8_t *)header.bytes)[0];
                    if (kind == 3) break;
                    __block NSError *commandError = nil;
                    dispatch_sync(dispatch_get_main_queue(), ^{
                        if (kind == 1) [host playback:payload error:&commandError];
                        else if (kind == 2) [host clear:payload error:&commandError];
                        else commandError = audioError(@"Unknown audio command");
                    });
                    if (commandError) { [host fail:commandError.localizedDescription]; return; }
                }
                dispatch_async(dispatch_get_main_queue(), ^{ [host stop]; exit(0); });
            }
        });
        [[NSRunLoop mainRunLoop] run];
    }
    return 0;
}
