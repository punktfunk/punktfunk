// PQ BT.2020 → SDR BT.709 for an 8-bit sRGB target, shared by nv12_csc.frag and planar_csc.frag.
// The same curve as the host's gamescope capture (patch 0017) and the Apple presenter: BT.2390
// EETF in PQ from a `peak`×203-nit source onto 203-nit SDR white, applied to max(R,G,B) in
// linear BT.709 so hue holds. Below ~88 nits (1000-nit source) it is the identity.

// SMPTE ST.2084 (PQ) EOTF: code value → linear, 1.0 = 10000 nits.
vec3 pq_eotf(vec3 e) {
    const float m1 = 0.1593017578125;  // 2610/16384
    const float m2 = 78.84375;         // 2523/4096 * 128
    const float c1 = 0.8359375;        // 3424/4096
    const float c2 = 18.8515625;       // 2413/4096 * 32
    const float c3 = 18.6875;          // 2392/4096 * 32
    vec3 p = pow(max(e, vec3(0.0)), vec3(1.0 / m2));
    return pow(max(p - c1, vec3(0.0)) / (c2 - c3 * p), vec3(1.0 / m1));
}

// Inverse of pq_eotf for one channel: linear (1.0 = 10000 nits) → code value.
float pq_oetf(float y) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float p = pow(clamp(y, 0.0, 1.0), m1);
    return pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
}

// BT.2020 → BT.709 primaries (linear light).
vec3 bt2020_to_709(vec3 c) {
    return mat3(
         1.6605, -0.1246, -0.0182,
        -0.5876,  1.1329, -0.1006,
        -0.0728, -0.0083,  1.1187
    ) * c;
}

// Linear → sRGB OETF.
vec3 srgb_oetf(vec3 c) {
    c = clamp(c, 0.0, 1.0);
    bvec3 lo = lessThanEqual(c, vec3(0.0031308));
    vec3 hi = 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055;
    return mix(hi, c * 12.92, vec3(lo));
}

// `pq`: full-range PQ R′G′B′ (BT.2020). `peak`: source peak in 203-nit units.
vec3 pq_to_sdr(vec3 pq, float peak) {
    vec3 lin = max(bt2020_to_709(pq_eotf(clamp(pq, 0.0, 1.0))), vec3(0.0));
    float l = max(lin.r, max(lin.g, lin.b));
    if (l > 0.0) {
        const float white = 203.0 / 10000.0;
        float src = pq_oetf(max(peak, 1.0001) * white);
        float max_lum = pq_oetf(white) / src;
        float ks = 1.5 * max_lum - 0.5;
        float e = min(pq_oetf(l) / src, 1.0);
        if (e > ks) {
            float t = (e - ks) / (1.0 - ks);
            float t2 = t * t;
            float t3 = t2 * t;
            e = (2.0 * t3 - 3.0 * t2 + 1.0) * ks + (t3 - 2.0 * t2 + t) * (1.0 - ks)
                + (-2.0 * t3 + 3.0 * t2) * max_lum;
        }
        lin *= pq_eotf(vec3(e * src)).r / l;
    }
    return srgb_oetf(lin / (203.0 / 10000.0));
}
