#include <errno.h>
#include <inttypes.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavformat/avio.h>
#include <libavutil/error.h>
#include <libavutil/pixfmt.h>
#include <libswscale/swscale.h>

typedef int (*chatt_ffmpeg_read_fn)(void *opaque, uint8_t *buffer, int length);
typedef int64_t (*chatt_ffmpeg_seek_fn)(void *opaque, int64_t offset, int whence);

struct chatt_ffmpeg_thumbnail {
    int width;
    int height;
    double duration;
};

struct chatt_ffmpeg_io {
    void *opaque;
    int64_t byte_len;
    chatt_ffmpeg_read_fn read;
    chatt_ffmpeg_seek_fn seek;
};

static int set_error(char *output, size_t capacity, const char *format, ...)
{
    if (output && capacity > 0) {
        va_list args;
        va_start(args, format);
        vsnprintf(output, capacity, format, args);
        va_end(args);
        output[capacity - 1] = '\0';
    }
    return -1;
}

static int set_av_error(char *output, size_t capacity, const char *operation,
                        int error)
{
    char detail[AV_ERROR_MAX_STRING_SIZE] = {0};
    av_strerror(error, detail, sizeof(detail));
    return set_error(output, capacity, "%s: %s", operation, detail);
}

static int read_packet(void *opaque, uint8_t *buffer, int length)
{
    struct chatt_ffmpeg_io *io = opaque;
    int read = io->read(io->opaque, buffer, length);
    if (read < 0)
        return AVERROR(EIO);
    if (read == 0)
        return AVERROR_EOF;
    return read;
}

static int64_t seek(void *opaque, int64_t offset, int whence)
{
    struct chatt_ffmpeg_io *io = opaque;
    if ((whence & ~AVSEEK_FORCE) == AVSEEK_SIZE)
        return io->byte_len;
    whence &= ~AVSEEK_FORCE;
    if (whence != SEEK_SET && whence != SEEK_CUR && whence != SEEK_END)
        return AVERROR(EINVAL);
    int64_t position = io->seek(io->opaque, offset, whence);
    return position < 0 ? AVERROR(EIO) : position;
}

static void bounded_size(int source_width, int source_height, int maximum_width,
                         int maximum_height, int *width, int *height)
{
    *width = source_width;
    *height = source_height;
    if (source_width <= maximum_width && source_height <= maximum_height)
        return;

    if ((int64_t)maximum_width * source_height <=
        (int64_t)maximum_height * source_width) {
        *width = maximum_width;
        *height = (int)(((int64_t)source_height * maximum_width +
                         source_width / 2) /
                        source_width);
    } else {
        *height = maximum_height;
        *width = (int)(((int64_t)source_width * maximum_height +
                        source_height / 2) /
                       source_height);
    }
    if (*width < 1)
        *width = 1;
    if (*height < 1)
        *height = 1;
}

int chatt_ffmpeg_extract_first_frame(
    void *opaque, int64_t byte_len, chatt_ffmpeg_read_fn read,
    chatt_ffmpeg_seek_fn seek_callback, int maximum_width, int maximum_height,
    uint8_t *bgra, size_t bgra_capacity,
    struct chatt_ffmpeg_thumbnail *thumbnail, char *error,
    size_t error_capacity)
{
    const int io_buffer_size = 64 * 1024;
    struct chatt_ffmpeg_io io = {
        .opaque = opaque,
        .byte_len = byte_len,
        .read = read,
        .seek = seek_callback,
    };
    AVFormatContext *format = NULL;
    AVIOContext *avio = NULL;
    AVCodecContext *decoder_context = NULL;
    AVPacket *packet = NULL;
    AVFrame *frame = NULL;
    struct SwsContext *scaler = NULL;
    int result = -1;

    if (!opaque || byte_len <= 0 || !read || !seek_callback ||
        maximum_width <= 0 || maximum_height <= 0 || !bgra || !thumbnail)
        return set_error(error, error_capacity,
                         "invalid FFmpeg thumbnail arguments");
    memset(thumbnail, 0, sizeof(*thumbnail));

    uint8_t *avio_buffer = av_malloc(io_buffer_size);
    if (!avio_buffer)
        return set_error(error, error_capacity,
                         "allocate FFmpeg input buffer: out of memory");
    avio = avio_alloc_context(avio_buffer, io_buffer_size, 0, &io, read_packet,
                              NULL, seek);
    if (!avio) {
        av_free(avio_buffer);
        return set_error(error, error_capacity,
                         "create FFmpeg input context: out of memory");
    }
    avio->seekable = AVIO_SEEKABLE_NORMAL;

    format = avformat_alloc_context();
    if (!format) {
        set_error(error, error_capacity,
                  "allocate FFmpeg demuxer: out of memory");
        goto cleanup;
    }
    format->pb = avio;
    format->flags |= AVFMT_FLAG_CUSTOM_IO;

    int status = avformat_open_input(&format, NULL, NULL, NULL);
    if (status < 0) {
        set_av_error(error, error_capacity, "open thumbnail input", status);
        goto cleanup;
    }
    status = avformat_find_stream_info(format, NULL);
    if (status < 0) {
        set_av_error(error, error_capacity, "inspect thumbnail streams", status);
        goto cleanup;
    }

    const AVCodec *decoder = NULL;
    int video_stream =
        av_find_best_stream(format, AVMEDIA_TYPE_VIDEO, -1, -1, &decoder, 0);
    if (video_stream < 0) {
        set_av_error(error, error_capacity, "find thumbnail video stream",
                     video_stream);
        goto cleanup;
    }
    AVStream *stream = format->streams[video_stream];
    decoder_context = avcodec_alloc_context3(decoder);
    if (!decoder_context) {
        set_error(error, error_capacity,
                  "allocate thumbnail decoder: out of memory");
        goto cleanup;
    }
    status = avcodec_parameters_to_context(decoder_context, stream->codecpar);
    if (status < 0) {
        set_av_error(error, error_capacity, "configure thumbnail decoder",
                     status);
        goto cleanup;
    }
    decoder_context->pkt_timebase = stream->time_base;
    decoder_context->thread_count = 1;
    status = avcodec_open2(decoder_context, decoder, NULL);
    if (status < 0) {
        set_av_error(error, error_capacity, "open thumbnail decoder", status);
        goto cleanup;
    }

    packet = av_packet_alloc();
    frame = av_frame_alloc();
    if (!packet || !frame) {
        set_error(error, error_capacity,
                  "allocate thumbnail decode buffers: out of memory");
        goto cleanup;
    }

    int got_frame = 0;
    while (!got_frame && (status = av_read_frame(format, packet)) >= 0) {
        if (packet->stream_index == video_stream) {
            status = avcodec_send_packet(decoder_context, packet);
            if (status < 0 && status != AVERROR(EAGAIN)) {
                av_packet_unref(packet);
                set_av_error(error, error_capacity,
                             "submit thumbnail video packet", status);
                goto cleanup;
            }
            while ((status = avcodec_receive_frame(decoder_context, frame)) >=
                   0) {
                got_frame = 1;
                break;
            }
            if (!got_frame && status != AVERROR(EAGAIN) &&
                status != AVERROR_EOF) {
                av_packet_unref(packet);
                set_av_error(error, error_capacity,
                             "decode thumbnail video frame", status);
                goto cleanup;
            }
        }
        av_packet_unref(packet);
    }
    if (!got_frame) {
        status = avcodec_send_packet(decoder_context, NULL);
        if (status >= 0 || status == AVERROR_EOF)
            status = avcodec_receive_frame(decoder_context, frame);
        if (status < 0) {
            set_av_error(error, error_capacity,
                         "video ended before a thumbnail frame was decoded",
                         status);
            goto cleanup;
        }
        got_frame = 1;
    }

    if (frame->width <= 0 || frame->height <= 0) {
        set_error(error, error_capacity,
                  "thumbnail decoder returned invalid dimensions");
        goto cleanup;
    }
    int width;
    int height;
    bounded_size(frame->width, frame->height, maximum_width, maximum_height,
                 &width, &height);
    size_t stride = (size_t)width * 4;
    if ((size_t)height > SIZE_MAX / stride ||
        stride * (size_t)height > bgra_capacity) {
        set_error(error, error_capacity,
                  "thumbnail output buffer is too small");
        goto cleanup;
    }

    enum AVPixelFormat source_format = frame->format;
    int source_full_range = frame->color_range == AVCOL_RANGE_JPEG;
    switch (source_format) {
    case AV_PIX_FMT_YUVJ420P:
        source_format = AV_PIX_FMT_YUV420P;
        source_full_range = 1;
        break;
    case AV_PIX_FMT_YUVJ422P:
        source_format = AV_PIX_FMT_YUV422P;
        source_full_range = 1;
        break;
    case AV_PIX_FMT_YUVJ444P:
        source_format = AV_PIX_FMT_YUV444P;
        source_full_range = 1;
        break;
    case AV_PIX_FMT_YUVJ440P:
        source_format = AV_PIX_FMT_YUV440P;
        source_full_range = 1;
        break;
    default:
        break;
    }
    scaler = sws_getContext(frame->width, frame->height, source_format, width,
                            height, AV_PIX_FMT_BGRA, SWS_BILINEAR, NULL, NULL,
                            NULL);
    if (!scaler) {
        set_error(error, error_capacity,
                  "create thumbnail scaler: unsupported pixel format");
        goto cleanup;
    }
    const int *coefficients = sws_getCoefficients(SWS_CS_DEFAULT);
    status = sws_setColorspaceDetails(scaler, coefficients, source_full_range,
                                      coefficients, 1, 0, 1 << 16, 1 << 16);
    if (status < 0) {
        set_av_error(error, error_capacity,
                     "configure thumbnail color conversion", status);
        goto cleanup;
    }
    uint8_t *destination[] = {bgra, NULL, NULL, NULL};
    int destination_stride[] = {(int)stride, 0, 0, 0};
    status = sws_scale(scaler, (const uint8_t *const *)frame->data,
                       frame->linesize, 0, frame->height, destination,
                       destination_stride);
    if (status != height) {
        set_error(error, error_capacity,
                  "thumbnail scaler returned %d rows instead of %d", status,
                  height);
        goto cleanup;
    }

    thumbnail->width = width;
    thumbnail->height = height;
    if (format->duration != AV_NOPTS_VALUE)
        thumbnail->duration = (double)format->duration / AV_TIME_BASE;
    else if (stream->duration != AV_NOPTS_VALUE)
        thumbnail->duration =
            stream->duration * av_q2d(stream->time_base);
    result = 0;

cleanup:
    sws_freeContext(scaler);
    av_frame_free(&frame);
    av_packet_free(&packet);
    avcodec_free_context(&decoder_context);
    avformat_close_input(&format);
    avio_context_free(&avio);
    return result;
}
