/**
 * Склейка капчи: порядок как flayercaptcha (viewDirection + mapping),
 * поворот 90° × rotate.
 */
import { createRequire } from 'module';
import { PNG } from 'pngjs';

const require = createRequire(import.meta.url);
const ItemUtils = require('flayercaptcha/utils/itemUtils.js');
const ImageUtils = require('flayercaptcha/utils/imageUtils.js');

const SIZE = 128;

function rgbaFromMapBin(buf) {
    // flayercaptcha palette → Uint8ClampedArray length 65536
    const clamped = ItemUtils.convertToImageBuffer(buf);
    return Buffer.from(clamped.buffer, clamped.byteOffset, clamped.byteLength);
}

/** Поворот на k*90° по часовой (k = rotate metadata). */
function rotateRgba90(src, turns) {
    const k = ((turns % 4) + 4) % 4;
    if (k === 0) return Buffer.from(src);
    let cur = Buffer.from(src);
    for (let t = 0; t < k; t++) {
        const next = Buffer.alloc(SIZE * SIZE * 4);
        for (let y = 0; y < SIZE; y++) {
            for (let x = 0; x < SIZE; x++) {
                // 90° CW: (x,y) → (SIZE-1-y, x)
                const nx = SIZE - 1 - y;
                const ny = x;
                const si = (y * SIZE + x) * 4;
                const di = (ny * SIZE + nx) * 4;
                next[di] = cur[si];
                next[di + 1] = cur[si + 1];
                next[di + 2] = cur[si + 2];
                next[di + 3] = cur[si + 3];
            }
        }
        cur = next;
    }
    return cur;
}

/**
 * @param {Array<{mapId:number, rotate:number, coordinate:{x,y,z}, viewDirection:string}>} frames
 * @param {Map<number, Buffer>} mapsRaw
 */
export async function assembleCaptcha(frames, mapsRaw) {
    if (!frames.length) throw new Error('no frames');

    const viewDirection = frames[0].viewDirection;
    const data = { x: [], y: [], z: [], coordinates: new Map() };

    for (const f of frames) {
        const { x, y, z } = f.coordinate;
        data.x.push(x);
        data.y.push(y);
        data.z.push(z);
        data.coordinates.set(f.coordinate, f);
    }

    // координаты как ключи Map сравниваются по ссылке — пересоберём по строковому ключу
    const byKey = new Map();
    for (const f of frames) {
        const key = `${f.coordinate.x},${f.coordinate.y},${f.coordinate.z}`;
        byKey.set(key, f);
    }

    const { widthMapping, heightMapping } = ImageUtils.getImageMapping(data, viewDirection);
    const { widthKey, heightKey } = ImageUtils.getImageKeys(data, viewDirection);
    const { width, height } = ImageUtils.getImageSize(data, viewDirection);

    const canvas = new PNG({ width, height });
    canvas.data.fill(0);

    const placed = [];
    for (const f of frames) {
        const bin = mapsRaw.get(f.mapId);
        if (!bin) continue;

        let rot = f.rotate ?? 0;
        if (viewDirection === 'up') rot = rot - 2;

        const rgba = rotateRgba90(rgbaFromMapBin(bin), rot);
        const left = widthMapping.get(f.coordinate[widthKey]);
        const top = heightMapping.get(f.coordinate[heightKey]);
        if (left == null || top == null) continue;

        for (let y = 0; y < SIZE; y++) {
            for (let x = 0; x < SIZE; x++) {
                const si = (y * SIZE + x) * 4;
                const di = ((top + y) * width + (left + x)) * 4;
                if (rgba[si + 3] === 0) continue;
                canvas.data[di] = rgba[si];
                canvas.data[di + 1] = rgba[si + 1];
                canvas.data[di + 2] = rgba[si + 2];
                canvas.data[di + 3] = rgba[si + 3];
            }
        }
        placed.push({
            mapId: f.mapId,
            rotate: f.rotate,
            left,
            top,
            ...f.coordinate,
        });
    }

    return {
        png: PNG.sync.write(canvas),
        layout: {
            viewDirection,
            width,
            height,
            widthKey,
            heightKey,
            placed,
            capturedAt: new Date().toISOString(),
        },
    };
}
