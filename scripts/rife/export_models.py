"""Exports the RIFE models used by the frame generation (rife/models/*.onnx).

Usage:
    pip install torch onnx
    python export_models.py --weights <folder with flownet_v4.6.pkl and flownet_v4.26.pkl> --out <folder>

Weights are the Practical-RIFE ones (MIT), named like in vs-rife:
flownet_v4.6.pkl, flownet_v4.26.pkl. The network code in rifenet/ is from
vs-rife (MIT, see rifenet/LICENSE).

Every model takes two RGBA8 frames [1,H,W,4] and outputs fp16 [1,H/2,W/2,8]:
motion to frame 0 (x,y), motion to frame 1 (x,y), blend mask, 3x padding.
TackleCast builds the in-between frame itself from that (the blend mask of
RIFE 4.6 has a 2 px checkerboard pattern that shows up as shimmer if you use
the network's own output image).

    rife46fm   RIFE 4.6, motion estimated at full resolution
    rife46fmh  RIFE 4.6, motion estimated on frames scaled down 2x
    rife426fmh RIFE 4.26, motion estimated on frames scaled down 2x
"""
import argparse
import os
import sys

import torch
import torch.nn as nn
import torch.nn.functional as F

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from rifenet.warplayer import warp  # noqa: E402

SIZES = [(2560, 1440), (1920, 1080), (1280, 720)]


def load_weights(path):
    sd = torch.load(path, map_location="cpu")
    return {k.replace("module.", ""): v for k, v in sd.items() if "module." in k}


class RifeFlow(nn.Module):
    def __init__(self, version, weights_dir, width, height, half):
        super().__init__()
        self.half_res = half
        self.out_w, self.out_h = width // 2, height // 2
        # resolution the network runs at
        self.w, self.h = (width // 2, height // 2) if half else (width, height)
        if version == "4.6":
            from rifenet.IFNet_HDv3_v4_6 import IFNet

            self.net = IFNet(1.0, False)
            self.net.load_state_dict(load_weights(os.path.join(weights_dir, "flownet_v4.6.pkl")), strict=False)
            self.enc = None
            mod = 32
        else:
            from rifenet.IFNet_HDv3_v4_26 import IFNet, Head

            sd = load_weights(os.path.join(weights_dir, "flownet_v4.26.pkl"))
            self.net = IFNet(1.0, False)
            self.net.load_state_dict(sd, strict=False)
            self.enc = Head()
            self.enc.load_state_dict({k.replace("encode.", ""): v for k, v in sd.items() if "encode." in k})
            mod = 64
        self.ph = -(-self.h // mod) * mod
        self.pw = -(-self.w // mod) * mod
        ph, pw = self.ph, self.pw
        self.register_buffer("div", torch.tensor([(pw - 1) / 2, (ph - 1) / 2]))
        gx = torch.linspace(-1, 1, pw).view(1, 1, 1, pw).expand(-1, -1, ph, -1)
        gy = torch.linspace(-1, 1, ph).view(1, 1, ph, 1).expand(-1, -1, -1, pw)
        self.register_buffer("grid", torch.cat([gx, gy], 1).contiguous())
        self.register_buffer("t", torch.full((1, 1, ph, pw), 0.5))

    def prep(self, x):
        x = x[..., :3].permute(0, 3, 1, 2).half() * (1.0 / 255.0)
        if self.half_res:
            x = F.avg_pool2d(x[:, :, : self.h * 2, : self.w * 2], 2)
        pad = (0, self.pw - self.w, 0, self.ph - self.h)
        return F.pad(x, pad) if any(pad) else x

    def flow_mask(self, a, b):
        n, t = self.net, self.t
        flow = mask = feat = None
        w0, w1 = a, b
        if self.enc is None:
            blocks = [n.block0, n.block1, n.block2, n.block3]
            for i, block in enumerate(blocks):
                if flow is None:
                    flow, mask = block(torch.cat((a, b, t), 1), None, scale=n.scale_list[i])
                else:
                    f, m = block(torch.cat((w0, w1, t, mask), 1), flow, scale=n.scale_list[i])
                    flow, mask = flow + f, mask + m
                if i < len(blocks) - 1:
                    w0 = warp(a, flow[:, :2], self.div, self.grid)
                    w1 = warp(b, flow[:, 2:4], self.div, self.grid)
        else:
            fa, fb = self.enc(a), self.enc(b)
            blocks = [n.block0, n.block1, n.block2, n.block3, n.block4]
            for i, block in enumerate(blocks):
                if flow is None:
                    flow, mask, feat = block(torch.cat((a, b, fa, fb, t), 1), None, scale=n.scale_list[i])
                else:
                    wf0 = warp(fa, flow[:, :2], self.div, self.grid)
                    wf1 = warp(fb, flow[:, 2:4], self.div, self.grid)
                    f, mask, feat = block(torch.cat((w0, w1, wf0, wf1, t, mask, feat), 1), flow, scale=n.scale_list[i])
                    flow = flow + f
                if i < len(blocks) - 1:
                    w0 = warp(a, flow[:, :2], self.div, self.grid)
                    w1 = warp(b, flow[:, 2:4], self.div, self.grid)
        return flow.float(), torch.sigmoid(mask.float())

    def forward(self, img0, img1):
        flow, mask = self.flow_mask(self.prep(img0), self.prep(img1))
        fm = torch.cat([flow, mask], 1)[:, :, : self.h, : self.w]
        if self.half_res:
            fm = torch.cat([fm[:, :4] * 2.0, fm[:, 4:]], 1)  # motion in full-res pixels
        else:
            fm = F.avg_pool2d(fm, 2)  # also removes the mask checkerboard
        fm = torch.cat([fm, torch.zeros_like(fm[:, :3])], 1)
        return fm.permute(0, 2, 3, 1).half()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--weights", required=True)
    ap.add_argument("--out", default="models")
    ap.add_argument("--models", nargs="+", choices=["rife426fmh", "rife46fmh", "rife46fm"],
                    default=["rife426fmh", "rife46fmh"])
    args = ap.parse_args()
    torch.set_num_threads(4)
    os.makedirs(args.out, exist_ok=True)
    for stem, version, half in (("rife426fmh", "4.26", True), ("rife46fmh", "4.6", True), ("rife46fm", "4.6", False)):
        if stem not in args.models:
            continue
        for w, h in SIZES:
            m = RifeFlow(version, args.weights, w, h, half).eval()
            # Never export an apparently valid model with uninitialised weights.
            weights = load_weights(os.path.join(args.weights, f"flownet_v{version}.pkl"))
            missing = set(m.net.state_dict()) - set(weights)
            if missing:
                raise RuntimeError(f"Missing RIFE {version} weights: {sorted(missing)}")
            m.net.half()
            m.t = m.t.half()
            if m.enc is not None:
                m.enc.half()
            x = torch.randint(0, 255, (1, h, w, 4), dtype=torch.uint8)
            path = os.path.join(args.out, f"{stem}_{w}x{h}.onnx")
            with torch.no_grad():
                torch.onnx.export(m, (x, x.clone()), path, input_names=["img0", "img1"],
                                  output_names=["out"], opset_version=17, dynamo=False)
            print(path)


if __name__ == "__main__":
    main()
