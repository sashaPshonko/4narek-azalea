/**
 * Качество собранной капчи: дырки + разрывы на швах тайлов 128×128.
 * Разрыв швов = чужие map / перемешанные рамки (фантайм так игроку не показывает).
 */
import { PNG } from 'pngjs';

export const TILE = 128;

/** Порог среднего |ΔRGB| на шве (labeled ≈15–29, каша обычно ≥60). */
export const DEFAULT_MAX_SEAM_MEAN = 32;
/** Пик по одному шву у нормальных тоже бывает высоким — держим свободно. */
export const DEFAULT_MAX_SEAM_MAX = 120;

function meanAbsRgb(a, b) {
    return (
        (Math.abs(a[0] - b[0]) + Math.abs(a[1] - b[1]) + Math.abs(a[2] - b[2])) / 3
    );
}

/**
 * @param {Buffer} pngBuf
 * @returns {{
 *   ok: boolean,
 *   cols: number,
 *   rows: number,
 *   blackRatio: number,
 *   seamMean: number,
 *   seamMax: number,
 *   hSeams: number[],
 *   vSeams: number[],
 *   reason?: string
 * }}
 */
export function inspectCaptchaPng(
    pngBuf,
    {
        maxBlackRatio = 0.04,
        maxSeamMean = DEFAULT_MAX_SEAM_MEAN,
        maxSeamMax = DEFAULT_MAX_SEAM_MAX,
    } = {},
) {
    let png;
    try {
        png = PNG.sync.read(pngBuf);
    } catch (e) {
        return {
            ok: false,
            cols: 0,
            rows: 0,
            blackRatio: 1,
            seamMean: 999,
            seamMax: 999,
            hSeams: [],
            vSeams: [],
            reason: `png: ${e.message || e}`,
        };
    }

    const { width, height, data } = png;
    if (width % TILE !== 0 || height % TILE !== 0) {
        return {
            ok: false,
            cols: 0,
            rows: 0,
            blackRatio: 1,
            seamMean: 999,
            seamMax: 999,
            hSeams: [],
            vSeams: [],
            reason: `size ${width}×${height} не кратно ${TILE}`,
        };
    }

    const cols = width / TILE;
    const rows = height / TILE;
    // FunTime: обычно 4×3; допускаем 3×4 на всякий.
    if (cols * rows < 12 || (cols !== 4 && rows !== 3 && !(cols === 3 && rows === 4))) {
        return {
            ok: false,
            cols,
            rows,
            blackRatio: 1,
            seamMean: 999,
            seamMax: 999,
            hSeams: [],
            vSeams: [],
            reason: `сетка ${cols}×${rows}, ждем 4×3`,
        };
    }

    let black = 0;
    const n = width * height;
    for (let i = 0; i < data.length; i += 4) {
        if (data[i] < 8 && data[i + 1] < 8 && data[i + 2] < 8) black += 1;
    }
    const blackRatio = n ? black / n : 1;

    const pix = (x, y) => {
        const i = (y * width + x) * 4;
        return [data[i], data[i + 1], data[i + 2]];
    };

    const hSeams = [];
    for (let r = 0; r < rows; r++) {
        for (let c = 0; c < cols - 1; c++) {
            let s = 0;
            const x0 = (c + 1) * TILE - 1;
            const x1 = (c + 1) * TILE;
            for (let y = 0; y < TILE; y++) {
                s += meanAbsRgb(pix(x0, r * TILE + y), pix(x1, r * TILE + y));
            }
            hSeams.push(s / TILE);
        }
    }

    const vSeams = [];
    for (let r = 0; r < rows - 1; r++) {
        for (let c = 0; c < cols; c++) {
            let s = 0;
            const y0 = (r + 1) * TILE - 1;
            const y1 = (r + 1) * TILE;
            for (let x = 0; x < TILE; x++) {
                s += meanAbsRgb(pix(c * TILE + x, y0), pix(c * TILE + x, y1));
            }
            vSeams.push(s / TILE);
        }
    }

    const all = [...hSeams, ...vSeams];
    const seamMean = all.length ? all.reduce((a, b) => a + b, 0) / all.length : 999;
    const seamMax = all.length ? Math.max(...all) : 999;

    if (blackRatio > maxBlackRatio) {
        return {
            ok: false,
            cols,
            rows,
            blackRatio,
            seamMean,
            seamMax,
            hSeams,
            vSeams,
            reason: `дырки black=${(blackRatio * 100).toFixed(1)}%`,
        };
    }
    if (seamMean > maxSeamMean || seamMax > maxSeamMax) {
        return {
            ok: false,
            cols,
            rows,
            blackRatio,
            seamMean,
            seamMax,
            hSeams,
            vSeams,
            reason: `швы mean=${seamMean.toFixed(1)} max=${seamMax.toFixed(1)}`,
        };
    }

    return {
        ok: true,
        cols,
        rows,
        blackRatio,
        seamMean,
        seamMax,
        hSeams,
        vSeams,
    };
}

export function isGoodCaptchaPng(pngBuf, opts) {
    return inspectCaptchaPng(pngBuf, opts).ok;
}

/**
 * Стена капчи: одна плоскость + прямоугольная сетка без дырок.
 * @param {Array<{coordinate:{x,y,z}, viewDirection:string, mapId:number}>} frames
 * @param {{x,y,z}|null} origin
 * @param {number} minFrames
 */
export function pickCaptchaWallFrames(frames, origin, minFrames = 12) {
    if (!frames?.length || frames.length < minFrames) return null;

    const byDir = new Map();
    for (const f of frames) {
        const d = f.viewDirection || 'north';
        if (!byDir.has(d)) byDir.set(d, []);
        byDir.get(d).push(f);
    }

    /** @type {{ frames: typeof frames, score: number }[]} */
    const candidates = [];

    for (const [dir, list] of byDir) {
        if (list.length < minFrames) continue;

        const wallKey =
            dir === 'east' || dir === 'west'
                ? 'x'
                : dir === 'up' || dir === 'down'
                  ? 'y'
                  : 'z';
        const widthKey = wallKey === 'x' ? 'z' : wallKey === 'z' ? 'x' : 'x';
        const heightKey = wallKey === 'y' ? 'z' : 'y';

        const planes = new Map();
        for (const f of list) {
            const wk = Math.round(f.coordinate[wallKey]);
            if (!planes.has(wk)) planes.set(wk, []);
            planes.get(wk).push(f);
        }

        for (const plane of planes.values()) {
            if (plane.length < minFrames) continue;

            // уникальные mapId — иначе чужой/старый tile
            const mapIds = new Set(plane.map((f) => f.mapId));
            if (mapIds.size < minFrames) continue;

            const colVals = [...new Set(plane.map((f) => Math.round(f.coordinate[widthKey])))].sort(
                (a, b) => a - b,
            );
            const rowVals = [
                ...new Set(plane.map((f) => Math.round(f.coordinate[heightKey]))),
            ].sort((a, b) => b - a);

            // плотная прямоугольная решётка
            const cells = new Set(
                plane.map(
                    (f) =>
                        `${Math.round(f.coordinate[widthKey])},${Math.round(f.coordinate[heightKey])}`,
                ),
            );
            if (cells.size !== plane.length) continue; // два frame в одной клетке

            let filled = 0;
            for (const c of colVals) {
                for (const r of rowVals) {
                    if (cells.has(`${c},${r}`)) filled += 1;
                }
            }
            if (filled !== colVals.length * rowVals.length) continue;
            if (filled < minFrames) continue;

            const cols = colVals.length;
            const rows = rowVals.length;

            let subset = plane;
            if (filled > minFrames) {
                subset = nearestRectSubset(plane, origin, widthKey, heightKey, minFrames);
                if (!subset || subset.length < minFrames) continue;
            } else {
                const shapeOk =
                    (cols === 4 && rows === 3) ||
                    (cols === 3 && rows === 4) ||
                    filled === minFrames;
                if (!shapeOk) continue;
            }

            const dist = origin
                ? subset.reduce(
                      (s, f) =>
                          s +
                          Math.hypot(
                              f.coordinate.x - origin.x,
                              f.coordinate.y - origin.y,
                              f.coordinate.z - origin.z,
                          ),
                      0,
                  ) / subset.length
                : 0;

            const exact =
                (cols === 4 && rows === 3) || (cols === 3 && rows === 4) ? 1000 : 0;
            candidates.push({
                frames: subset,
                score: exact + subset.length * 10 - dist,
            });
        }
    }

    if (!candidates.length) return null;
    candidates.sort((a, b) => b.score - a.score);
    return candidates[0].frames;
}

function nearestRectSubset(plane, origin, widthKey, heightKey, minFrames) {
    // ожидаем 4×3
    const wantCols = 4;
    const wantRows = 3;
    if (wantCols * wantRows !== minFrames) {
        return [...plane]
            .sort((a, b) => {
                if (!origin) return 0;
                const da = Math.hypot(
                    a.coordinate.x - origin.x,
                    a.coordinate.y - origin.y,
                    a.coordinate.z - origin.z,
                );
                const db = Math.hypot(
                    b.coordinate.x - origin.x,
                    b.coordinate.y - origin.y,
                    b.coordinate.z - origin.z,
                );
                return da - db;
            })
            .slice(0, minFrames);
    }

    const colVals = [...new Set(plane.map((f) => Math.round(f.coordinate[widthKey])))].sort(
        (a, b) => a - b,
    );
    const rowVals = [...new Set(plane.map((f) => Math.round(f.coordinate[heightKey])))].sort(
        (a, b) => b - a,
    );
    if (colVals.length < wantCols || rowVals.length < wantRows) return null;

    const byCell = new Map();
    for (const f of plane) {
        byCell.set(
            `${Math.round(f.coordinate[widthKey])},${Math.round(f.coordinate[heightKey])}`,
            f,
        );
    }

    let best = null;
    let bestDist = Infinity;
    for (let ci = 0; ci <= colVals.length - wantCols; ci++) {
        for (let ri = 0; ri <= rowVals.length - wantRows; ri++) {
            const pick = [];
            let ok = true;
            for (let dc = 0; dc < wantCols && ok; dc++) {
                for (let dr = 0; dr < wantRows; dr++) {
                    const f = byCell.get(`${colVals[ci + dc]},${rowVals[ri + dr]}`);
                    if (!f) {
                        ok = false;
                        break;
                    }
                    pick.push(f);
                }
            }
            if (!ok || pick.length !== minFrames) continue;
            const dist = origin
                ? pick.reduce(
                      (s, f) =>
                          s +
                          Math.hypot(
                              f.coordinate.x - origin.x,
                              f.coordinate.y - origin.y,
                              f.coordinate.z - origin.z,
                          ),
                      0,
                  ) / pick.length
                : 0;
            if (dist < bestDist) {
                bestDist = dist;
                best = pick;
            }
        }
    }
    return best;
}
