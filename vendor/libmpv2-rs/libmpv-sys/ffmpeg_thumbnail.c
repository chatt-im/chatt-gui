#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <math.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavformat/avio.h>
#include <libavutil/display.h>
#include <libavutil/error.h>
#include <libavutil/frame.h>
#include <libavutil/mathematics.h>
#include <libavutil/pixfmt.h>
#include <libswscale/swscale.h>

/* FFmpeg only applies log_level_offset to levels at or above AV_LOG_FATAL, so
 * shifting those past AV_LOG_TRACE keeps thumbnail decode noise out of the
 * process-global av_log callback that mpv installs, while leaving AV_LOG_PANIC
 * intact. The diagnostic the caller actually surfaces is captured in `error`.
 * Only AVCodecContext exposes the field; AVFormatContext has no equivalent, so
 * demuxer-level messages still reach mpv's callback. */
#define CHATT_THUMBNAIL_LOG_OFFSET (AV_LOG_TRACE + 8 - AV_LOG_FATAL)

typedef int (*chatt_ffmpeg_read_fn)(void *opaque, uint8_t *buffer, int length);
typedef int64_t (*chatt_ffmpeg_seek_fn)(void *opaque, int64_t offset, int whence);
typedef int (*chatt_ffmpeg_interrupt_fn)(void *opaque);

struct chatt_ffmpeg_thumbnail {
    int width;
    int height;
    int rotate;
    int reserved;
    double duration;
};

struct chatt_ffmpeg_io {
    void *opaque;
    int64_t byte_len;
    chatt_ffmpeg_read_fn read;
    chatt_ffmpeg_seek_fn seek;
    chatt_ffmpeg_interrupt_fn interrupt;
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

static int check_interrupt(void *opaque)
{
    struct chatt_ffmpeg_io *io = opaque;
    return io->interrupt ? io->interrupt(io->opaque) : 0;
}

static void bounded_size(int64_t source_width, int64_t source_height,
                         int maximum_width, int maximum_height, int *width,
                         int *height)
{
    if (source_width < 1)
        source_width = 1;
    if (source_height < 1)
        source_height = 1;

    int64_t scaled_width = source_width;
    int64_t scaled_height = source_height;
    if (source_width > maximum_width || source_height > maximum_height) {
        if ((int64_t)maximum_width * source_height <=
            (int64_t)maximum_height * source_width) {
            scaled_width = maximum_width;
            scaled_height = (source_height * maximum_width +
                             source_width / 2) /
                            source_width;
        } else {
            scaled_height = maximum_height;
            scaled_width = (source_width * maximum_height +
                            source_height / 2) /
                           source_height;
        }
    }

    if (scaled_width < 1)
        scaled_width = 1;
    if (scaled_height < 1)
        scaled_height = 1;
    if (scaled_width > maximum_width)
        scaled_width = maximum_width;
    if (scaled_height > maximum_height)
        scaled_height = maximum_height;
    *width = (int)scaled_width;
    *height = (int)scaled_height;
}

/* Clockwise degrees the image must be rotated at display time, matching mpv's
 * demux_lavf.c so thumbnails agree with playback. The frame matrix wins over the
 * container one, as it does in mpv's mp_image_copy_attributes. */
static int display_rotation(const AVFrame *frame, const AVStream *stream)
{
    const int32_t *matrix = NULL;
    const AVFrameSideData *frame_side_data =
        av_frame_get_side_data(frame, AV_FRAME_DATA_DISPLAYMATRIX);
    if (frame_side_data && frame_side_data->size >= 9 * sizeof(int32_t)) {
        matrix = (const int32_t *)frame_side_data->data;
    } else {
        const AVPacketSideData *stream_side_data = av_packet_side_data_get(
            stream->codecpar->coded_side_data,
            stream->codecpar->nb_coded_side_data, AV_PKT_DATA_DISPLAYMATRIX);
        if (stream_side_data &&
            stream_side_data->size >= 9 * sizeof(int32_t))
            matrix = (const int32_t *)stream_side_data->data;
    }
    if (!matrix)
        return 0;

    double rotation = av_display_rotation_get(matrix);
    if (isnan(rotation))
        return 0;
    int rotate = (((int)(-rotation) % 360) + 360) % 360;
    return (((rotate + 45) / 90) * 90) % 360;
}

static void display_size(AVFormatContext *format, AVStream *stream,
                         AVFrame *frame, int64_t *width, int64_t *height)
{
    int64_t display_width = frame->width;
    int64_t display_height = frame->height;
    AVRational aspect = av_guess_sample_aspect_ratio(format, stream, frame);
    if (aspect.num > 0 && aspect.den > 0 && aspect.num != aspect.den) {
        if (aspect.num > aspect.den)
            display_width = av_rescale(display_width, aspect.num, aspect.den);
        else
            display_height = av_rescale(display_height, aspect.den, aspect.num);
    }
    *width = display_width < 1 ? 1
                               : (display_width > INT_MAX ? INT_MAX
                                                          : display_width);
    *height = display_height < 1 ? 1
                                 : (display_height > INT_MAX ? INT_MAX
                                                             : display_height);
}

/* SWS_CS_* deliberately share the AVCOL_SPC_* numbering, so the frame's
 * colorspace can be handed to sws_getCoefficients directly. The fallback for
 * unspecified content mirrors mpv's mp_csp_guess_colorspace. */
static int source_colorspace(const AVFrame *frame)
{
    if (frame->colorspace != AVCOL_SPC_UNSPECIFIED)
        return frame->colorspace;
    return (frame->width >= 1280 || frame->height > 576) ? SWS_CS_ITU709
                                                         : SWS_CS_ITU601;
}

int chatt_ffmpeg_extract_first_frame(
    void *opaque, int64_t byte_len, chatt_ffmpeg_read_fn read,
    chatt_ffmpeg_seek_fn seek_callback, chatt_ffmpeg_interrupt_fn interrupt,
    int maximum_width, int maximum_height, int64_t maximum_pixels,
    int64_t probesize, int64_t maximum_analyze_duration, uint8_t *bgra,
    size_t bgra_capacity, struct chatt_ffmpeg_thumbnail *thumbnail, char *error,
    size_t error_capacity)
{
    const int io_buffer_size = 64 * 1024;
    struct chatt_ffmpeg_io io = {
        .opaque = opaque,
        .byte_len = byte_len,
        .read = read,
        .seek = seek_callback,
        .interrupt = interrupt,
    };
    AVFormatContext *format = NULL;
    AVIOContext *avio = NULL;
    AVCodecContext *decoder_context = NULL;
    AVPacket *packet = NULL;
    AVFrame *frame = NULL;
    struct SwsContext *scaler = NULL;
    int result = -1;

    if (!opaque || byte_len <= 0 || !read || !seek_callback ||
        maximum_width <= 0 || maximum_height <= 0 || maximum_pixels <= 0 ||
        probesize <= 0 || maximum_analyze_duration <= 0 || !bgra || !thumbnail)
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
    format->probesize = probesize;
    format->max_analyze_duration = maximum_analyze_duration;
    format->fps_probe_size = 0;
    format->interrupt_callback.callback = check_interrupt;
    format->interrupt_callback.opaque = &io;

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
    decoder_context->max_pixels = maximum_pixels;
    decoder_context->log_level_offset = CHATT_THUMBNAIL_LOG_OFFSET;
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

    /* Scaling happens before rotation, so the maxima are swapped for quarter
     * turns to keep the rotated result inside the caller's bounds. */
    int rotate = display_rotation(frame, stream);
    int quarter_turn = rotate == 90 || rotate == 270;
    int64_t display_width;
    int64_t display_height;
    display_size(format, stream, frame, &display_width, &display_height);
    int width;
    int height;
    bounded_size(display_width, display_height,
                 quarter_turn ? maximum_height : maximum_width,
                 quarter_turn ? maximum_width : maximum_height, &width,
                 &height);
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
    status = sws_setColorspaceDetails(
        scaler, sws_getCoefficients(source_colorspace(frame)),
        source_full_range, sws_getCoefficients(SWS_CS_DEFAULT), 1, 0, 1 << 16,
        1 << 16);
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
    thumbnail->rotate = rotate;
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
