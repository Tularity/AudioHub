"""Regenerate the immutable wire-rate boundary bank (offline, never in builds).

Requires NumPy 1.26.4. Run on an approved test/build resource, passing the output
file explicitly. The normal product build only embeds the checked-in bank.
Algorithm: 20 ms finite Fourier basis, Blackman input/output windows, sampling
density weighting and a 1e-8 relative singular-value cutoff. Values at known
sample nodes are copied exactly. Rows store little-endian float32 coefficients.
"""
import argparse
import hashlib
import json
import struct
from pathlib import Path

import numpy as np


def generate():
    rates = (16000, 24000, 32000, 48000)
    payload = bytearray(b"AHRT0001") + struct.pack("<II", 192, 12)
    metadata = []
    for before in rates:
        for after in rates:
            if before == after:
                continue
            grid, period, band = 96000, 0.02, min(before, after)
            times = np.concatenate([
                np.arange(-before // 100, 0) * (grid // before),
                np.arange(after // 100) * (grid // after),
            ])
            seconds = times / grid
            half = int(band * 0.5 * period) + 2
            frequency = np.arange(-half, half + 1) / period
            density = np.sqrt(np.where(times < 0, band / before, band / after))
            basis = np.exp(2j * np.pi * seconds[:, None] * frequency[None, :]) * density[:, None]
            inverse = np.linalg.pinv(basis, rcond=1e-8)
            window = 0.42 + 0.5 * np.cos(2 * np.pi * seconds / period) + 0.08 * np.cos(4 * np.pi * seconds / period)
            payload += struct.pack("<III", before, after, 384)
            max_l1 = max_l2 = dc = 0.0
            for target in np.arange(384) * 2 - 384:
                exact = np.flatnonzero(times == target)
                if len(exact):
                    start, weights = int(exact[0]), np.array([1.0], dtype="<f4")
                else:
                    at = target / grid
                    output_window = 0.42 + 0.5 * np.cos(2 * np.pi * at / period) + 0.08 * np.cos(4 * np.pi * at / period)
                    row = np.real(np.exp(2j * np.pi * frequency * at) @ inverse) * density * window / output_window
                    row /= row.sum()
                    start, end = 0, len(row)
                    while start < end and abs(row[start]) < 1e-12:
                        start += 1
                    while end > start and abs(row[end - 1]) < 1e-12:
                        end -= 1
                    weights = row[start:end].astype("<f4")
                wide = weights.astype(float)
                max_l1 = max(max_l1, float(np.abs(wide).sum()))
                max_l2 = max(max_l2, float(np.linalg.norm(wide)))
                dc = max(dc, abs(float(wide.sum()) - 1))
                payload += struct.pack("<II", start, len(weights)) + weights.tobytes()
            assert max_l2 < 4.2 and dc < 2e-6
            metadata.append(dict(before=before, after=after, max_l1=max_l1, max_l2=max_l2, dc_error=dc))
    return payload, metadata


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    payload, metadata = generate()
    with args.output.open("xb") as output:
        output.write(payload)
    print(json.dumps(dict(sha256=hashlib.sha256(payload).hexdigest(), bytes=len(payload), tables=metadata), indent=2))
