# JPEG hardware-test fixture

`nvjpeg-422.jpg` is a synthetic 16x8 solid-color JPEG, generated with FFmpeg 8.1.3:

```sh
ffmpeg -f lavfi -i color=c=0x5078a0:s=16x8 -frames:v 1 -pix_fmt yuvj422p nvjpeg-422.jpg
```

It contains no capture-card or console content. The ignored NVIDIA integration tests use it to check owned decoding and DX12/CUDA shared-buffer decoding.
