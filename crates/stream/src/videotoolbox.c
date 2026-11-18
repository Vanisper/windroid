// H.264/AVCC 解码。所有 CoreFoundation 所有权都收在这层。
// https://developer.apple.com/documentation/videotoolbox/vtdecompressionsessiondecodeframe(_:samplebuffer:flags:framerefcon:infoflagsout:)
#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <VideoToolbox/VideoToolbox.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    CMVideoFormatDescriptionRef format;
    VTDecompressionSessionRef session;
} WindroidVideoDecoder;

typedef struct {
    uint8_t *pixels;
    uint32_t width, height;
    OSStatus status;
    int32_t copied;
} FrameOutput;

static void receive_image(void *unused, void *reference, OSStatus status,
                          VTDecodeInfoFlags flags, CVImageBufferRef image,
                          CMTime pts, CMTime duration) {
    (void)unused; (void)flags; (void)pts; (void)duration;
    FrameOutput *out = reference;
    out->status = status;
    if (status != noErr || image == NULL) return;
    size_t row = (size_t)out->width * 4;
    size_t stride = CVPixelBufferGetBytesPerRow(image);
    if (CVPixelBufferGetPixelFormatType(image) != kCVPixelFormatType_32BGRA ||
        CVPixelBufferGetWidth(image) != out->width ||
        CVPixelBufferGetHeight(image) != out->height || stride < row) {
        out->status = kVTVideoDecoderBadDataErr;
        return;
    }
    out->status = CVPixelBufferLockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
    if (out->status != noErr) return;
    const uint8_t *src = CVPixelBufferGetBaseAddress(image);
    if (src != NULL) {
        for (size_t y = 0; y < out->height; ++y) {
            memcpy(out->pixels + y * row, src + y * stride, row);
        }
        out->copied = 1;
    } else {
        out->status = kVTVideoDecoderBadDataErr;
    }
    CVPixelBufferUnlockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
}

void windroid_vt_free(WindroidVideoDecoder *decoder) {
    if (decoder->session != NULL) {
        VTDecompressionSessionInvalidate(decoder->session);
        CFRelease(decoder->session);
    }
    if (decoder->format != NULL) CFRelease(decoder->format);
    free(decoder);
}

OSStatus windroid_vt_new(const uint8_t *sps, size_t sps_len, const uint8_t *pps, size_t pps_len,
                         uint32_t width, uint32_t height, WindroidVideoDecoder **result) {
    WindroidVideoDecoder *decoder = calloc(1, sizeof(*decoder));
    if (decoder == NULL) return memFullErr;
    const uint8_t *sets[] = {sps, pps};
    size_t sizes[] = {sps_len, pps_len};
    OSStatus status = CMVideoFormatDescriptionCreateFromH264ParameterSets(
        NULL, 2, sets, sizes, 4, &decoder->format);
    if (status != noErr) goto fail;

    // SPS 可以按宏块补齐尺寸。限制解码开销并要求输出匹配协商的可见帧。
    CMVideoDimensions encoded = CMVideoFormatDescriptionGetDimensions(decoder->format);
    if (encoded.width <= 0 || encoded.height <= 0 || (uint32_t)encoded.width > width + 32 ||
        (uint32_t)encoded.height > height + 32) {
        status = kVTVideoDecoderBadDataErr;
        goto fail;
    }
    int32_t pixel_format = kCVPixelFormatType_32BGRA;
    CFNumberRef values[] = {
        CFNumberCreate(NULL, kCFNumberSInt32Type, &pixel_format),
        CFNumberCreate(NULL, kCFNumberSInt32Type, &width),
        CFNumberCreate(NULL, kCFNumberSInt32Type, &height),
    };
    const void *keys[] = {kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelBufferHeightKey};
    CFDictionaryRef attributes = CFDictionaryCreate(NULL, keys, (const void **)values, 3,
        &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    VTDecompressionOutputCallbackRecord callback = {receive_image, NULL};
    status = VTDecompressionSessionCreate(NULL, decoder->format, NULL, attributes, &callback, &decoder->session);
    CFRelease(attributes);
    for (size_t i = 0; i < 3; ++i) CFRelease(values[i]);
    if (status != noErr) goto fail;
    VTSessionSetProperty(decoder->session, kVTDecompressionPropertyKey_RealTime, kCFBooleanTrue);
    *result = decoder;
    return noErr;
fail:
    windroid_vt_free(decoder);
    return status;
}

OSStatus windroid_vt_decode(WindroidVideoDecoder *decoder, const uint8_t *avcc, size_t size,
                            uint32_t width, uint32_t height, uint8_t *pixels, int32_t *copied) {
    CMBlockBufferRef block = NULL;
    CMSampleBufferRef sample = NULL;
    // 压缩数据在调用期间由 Rust 持有。两个解码 flag 都清零，保证回调早于函数返回。
    OSStatus status = CMBlockBufferCreateWithMemoryBlock(NULL, (void *)avcc, size, kCFAllocatorNull,
        NULL, 0, size, 0, &block);
    if (status != noErr) return status;
    status = CMSampleBufferCreateReady(NULL, block, decoder->format, 1, 0, NULL, 1, &size, &sample);
    if (status == noErr) {
        FrameOutput out = {pixels, width, height, noErr, 0};
        status = VTDecompressionSessionDecodeFrame(decoder->session, sample, 0, &out, NULL);
        if (status == noErr) status = out.status;
        *copied = out.copied;
        CFRelease(sample);
    }
    CFRelease(block);
    return status;
}
