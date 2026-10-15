/* Query native V4L2 modes without configuring or streaming from the device.
 * Use the platform header for ioctl numbers, unions, padding and ABI sizes. */
#include <errno.h>
#include <fcntl.h>
#include <linux/videodev2.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

struct tc_mode
{
    uint32_t fourcc, width, height, numerator, denominator;
};
static int call_ioctl(int fd, unsigned long request, void *arg)
{
    int result;
    do
    {
        result = ioctl(fd, request, arg);
    } while (result < 0 && errno == EINTR);
    return result;
}
static int append(struct tc_mode *out, size_t capacity, size_t *count, uint32_t fmt, uint32_t w, uint32_t h,
                  struct v4l2_fract t)
{
    if (!t.numerator || !t.denominator)
        return 0;
    if (*count >= capacity)
        return -1;
    out[*count] = (struct tc_mode){fmt, w, h, t.numerator, t.denominator};
    ++*count;
    return 0;
}
static long double seconds(struct v4l2_fract t)
{
    return t.denominator ? (long double)t.numerator / t.denominator : 0;
}
static int intervals(int fd, uint32_t fmt, uint32_t width, uint32_t height, struct tc_mode *out, size_t capacity,
                     size_t *count)
{
    for (uint32_t index = 0; index < 4096; ++index)
    {
        struct v4l2_frmivalenum value = {0};
        value.index = index;
        value.pixel_format = fmt;
        value.width = width;
        value.height = height;
        if (call_ioctl(fd, VIDIOC_ENUM_FRAMEINTERVALS, &value) < 0)
            break;
        if (value.type == V4L2_FRMIVAL_TYPE_DISCRETE)
        {
            if (append(out, capacity, count, fmt, width, height, value.discrete) < 0)
                return -1;
        }
        else if (value.type == V4L2_FRMIVAL_TYPE_CONTINUOUS || value.type == V4L2_FRMIVAL_TYPE_STEPWISE)
        {
            struct v4l2_fract candidates[] = {{1, 24},           {1, 25},        {1, 30},        {1, 50},
                                              {1, 60},           {1, 75},        {1, 90},        {1, 100},
                                              {1, 120},          {1, 144},       {1, 165},       {1, 180},
                                              {1, 200},          {1, 240},       {1001, 24000},  {1001, 30000},
                                              {1001, 60000},     {1001, 120000}, {1001, 240000}, value.stepwise.min,
                                              value.stepwise.max};
            long double low = seconds(value.stepwise.min), high = seconds(value.stepwise.max),
                        step = seconds(value.stepwise.step);
            if (low <= 0 || high < low)
                break;
            for (size_t i = 0; i < sizeof(candidates) / sizeof(candidates[0]); ++i)
            {
                long double time = seconds(candidates[i]);
                if (time < low - 1e-12L || time > high + 1e-12L)
                    continue;
                if (value.type == V4L2_FRMIVAL_TYPE_STEPWISE)
                {
                    if (step <= 0)
                        continue;
                    long double n = (time - low) / step;
                    if (fabsl(n - roundl(n)) > 1e-7L)
                        continue;
                }
                if (append(out, capacity, count, fmt, width, height, candidates[i]) < 0)
                    return -1;
            }
            break;
        }
    }
    return 0;
}
int32_t tc_v4l2_modes(const char *path, struct tc_mode *out, size_t capacity, char *error, size_t error_size)
{
    int fd = open(path, O_RDONLY | O_NONBLOCK | O_CLOEXEC);
    if (fd < 0)
    {
        snprintf(error, error_size, "Cannot query %s: %s", path, strerror(errno));
        return -1;
    }
    struct v4l2_capability caps = {0};
    if (call_ioctl(fd, VIDIOC_QUERYCAP, &caps) < 0)
    {
        snprintf(error, error_size, "Cannot read V4L2 capabilities: %s", strerror(errno));
        close(fd);
        return -1;
    }
    uint32_t flags = (caps.capabilities & V4L2_CAP_DEVICE_CAPS) ? caps.device_caps : caps.capabilities;
    if (!(flags & V4L2_CAP_VIDEO_CAPTURE))
    {
        snprintf(error, error_size, "This node does not provide single-plane video capture");
        close(fd);
        return -1;
    }
    size_t count = 0;
    for (uint32_t i = 0; i < 4096; ++i)
    {
        struct v4l2_fmtdesc fmt = {0};
        fmt.index = i;
        fmt.type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        if (call_ioctl(fd, VIDIOC_ENUM_FMT, &fmt) < 0)
            break;
        if (fmt.pixelformat != V4L2_PIX_FMT_NV12 && fmt.pixelformat != V4L2_PIX_FMT_MJPEG &&
            fmt.pixelformat != V4L2_PIX_FMT_YUYV && fmt.pixelformat != V4L2_PIX_FMT_UYVY &&
            fmt.pixelformat != V4L2_PIX_FMT_YUV420)
            continue;
        for (uint32_t j = 0; j < 4096; ++j)
        {
            struct v4l2_frmsizeenum size = {0};
            size.index = j;
            size.pixel_format = fmt.pixelformat;
            if (call_ioctl(fd, VIDIOC_ENUM_FRAMESIZES, &size) < 0)
                break;
            if (size.type == V4L2_FRMSIZE_TYPE_DISCRETE)
            {
                if (intervals(fd, fmt.pixelformat, size.discrete.width, size.discrete.height, out, capacity, &count) <
                    0)
                    goto overflow;
            }
            else if (size.type == V4L2_FRMSIZE_TYPE_STEPWISE || size.type == V4L2_FRMSIZE_TYPE_CONTINUOUS)
            {
                uint32_t candidates[][2] = {{640, 480},
                                            {720, 480},
                                            {720, 576},
                                            {1280, 720},
                                            {1920, 1080},
                                            {2560, 1440},
                                            {3840, 2160},
                                            {size.stepwise.min_width, size.stepwise.min_height},
                                            {size.stepwise.max_width, size.stepwise.max_height}};
                for (size_t k = 0; k < sizeof(candidates) / sizeof(candidates[0]); ++k)
                {
                    uint32_t w = candidates[k][0], h = candidates[k][1];
                    if (w < size.stepwise.min_width || w > size.stepwise.max_width || h < size.stepwise.min_height ||
                        h > size.stepwise.max_height)
                        continue;
                    if (size.type == V4L2_FRMSIZE_TYPE_STEPWISE &&
                        ((size.stepwise.step_width && (w - size.stepwise.min_width) % size.stepwise.step_width) ||
                         (size.stepwise.step_height && (h - size.stepwise.min_height) % size.stepwise.step_height)))
                        continue;
                    if (intervals(fd, fmt.pixelformat, w, h, out, capacity, &count) < 0)
                        goto overflow;
                }
                break;
            }
        }
    }
    close(fd);
    return (int32_t)count;
overflow:
    snprintf(error, error_size, "The device reported more capture modes than the supported query limit");
    close(fd);
    return -1;
}
