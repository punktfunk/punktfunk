/*
 * punktfunk-core C ABI harness — M1 acceptance.
 *
 * Proves the core links from C and round-trips encoded access units through the full
 * packetize -> FEC -> in-process loopback (with deterministic packet loss) -> FEC
 * recover -> reassemble path, recovering every byte exactly.
 *
 * Build/run: see tests/c/run.sh (also driven by `cargo test --test c_abi`).
 */
#include "punktfunk_core.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static PunktfunkConfig make_config(uint32_t role, uint32_t drop_period) {
    PunktfunkConfig c;
    memset(&c, 0, sizeof(c));
    c.struct_size = (uint32_t)sizeof(PunktfunkConfig);
    c.role = role;                 /* 0 = host, 1 = client */
    c.phase = 1;                   /* P1, GameStream-compatible */
    c.fec_scheme = 0;              /* GF(2^8) */
    c.fec_percent = 25;
    c.max_data_per_block = 64;
    c.shard_payload = 1024;
    c.max_frame_bytes = 8 * 1024 * 1024;
    c.encrypt = 0;
    c.loopback_drop_period = drop_period;
    return c;
}

int main(void) {
    printf("punktfunk-core C ABI harness (abi_version=%u)\n", punktfunk_abi_version());

    /* PunktfunkConnectOpts (v35): the C compiler must agree with Rust's const-asserted layout —
     * 104 bytes on 64-bit / 76 on 32-bit, NO tail padding (the growth contract: an appended field
     * may never land in bytes an older caller's sizeof already covered, and C leaves padding
     * unspecified, which is what `reserved0` exists to prevent) — and the size-prefix guard must
     * reject an undersized struct as a status, not a read. The declaration sits behind the
     * header's quic guard; the staticlib this harness links always carries quic (see the
     * -lopus/Security link line), so the check only needs the define. */
#ifdef PUNKTFUNK_FEATURE_QUIC
    if (sizeof(PunktfunkConnectOpts) != (sizeof(void *) == 8 ? 104u : 76u)) {
        fprintf(stderr, "FAIL: PunktfunkConnectOpts is %zu bytes\n", sizeof(PunktfunkConnectOpts));
        return 1;
    }
    {
        PunktfunkConnectOpts o;
        int32_t st = 0;
        memset(&o, 0, sizeof(o));
        o.struct_size = 4; /* an impossible, pre-v26 size */
        if (punktfunk_connect_opts(&o, NULL, &st) != NULL || st != PUNKTFUNK_STATUS_INVALID_ARG) {
            fprintf(stderr, "FAIL: undersized connect opts accepted (st=%d)\n", (int)st);
            return 1;
        }
    }
#endif

    {
        PunktfunkH265Concealer *concealer = punktfunk_h265_concealer_new();
        PunktfunkConcealment kind = PUNKTFUNK_CONCEALMENT_INTACT;
        uint8_t byte = 0;
        uint8_t *out = NULL;
        uintptr_t out_len = 0;
        PunktfunkStatus status = punktfunk_h265_concealer_conceal(
            concealer, &byte, SIZE_MAX, &kind, &out, &out_len);
        if (status != PUNKTFUNK_STATUS_INVALID_ARG) {
            fprintf(stderr, "FAIL: unrepresentable concealer input accepted (st=%d)\n",
                    (int)status);
            return 1;
        }
        punktfunk_h265_concealer_free(concealer);
    }

    {
        /* After a gap the rule withholds until the anchor, and refuses an unknown verdict. */
        AuAdmission *admit = punktfunk_au_admission_new();
        bool gap_held = false, anchor_held = true, ask = true;
        PunktfunkStatus s1 = punktfunk_au_admission_note(
            admit, 4, 1, 0, true, PUNKTFUNK_CONCEALED_NONE, &gap_held, &ask);
        PunktfunkStatus s2 = punktfunk_au_admission_note(
            admit, 5, 0, PUNKTFUNK_USER_FLAG_RECOVERY_ANCHOR, true, PUNKTFUNK_CONCEALED_NONE,
            &anchor_held, NULL);
        PunktfunkStatus s3 = punktfunk_au_admission_note(admit, 6, 0, 0, true, 7, NULL, NULL);
        if (s1 != PUNKTFUNK_STATUS_OK || s2 != PUNKTFUNK_STATUS_OK || !gap_held || ask
            || anchor_held || s3 != PUNKTFUNK_STATUS_INVALID_ARG) {
            fprintf(stderr, "FAIL: admission rule (st=%d/%d/%d)\n", (int)s1, (int)s2, (int)s3);
            return 1;
        }
        punktfunk_au_admission_free(admit);
    }

    const uint32_t DROP_PERIOD = 8;   /* drop 1 of every 8 packets */
    PunktfunkConfig host_cfg = make_config(0, DROP_PERIOD);
    PunktfunkConfig client_cfg = make_config(1, DROP_PERIOD);

    PunktfunkSession *host = NULL;
    PunktfunkSession *client = NULL;
    PunktfunkStatus rc = punktfunk_test_loopback_pair(&host_cfg, &client_cfg, &host, &client);
    if (rc != PUNKTFUNK_STATUS_OK || !host || !client) {
        fprintf(stderr, "FAIL: loopback_pair rc=%d\n", (int)rc);
        return 1;
    }

    const size_t FRAME_LEN = 200000;  /* ~196 shards across 4 FEC blocks */
    const int FRAMES = 4;
    uint8_t *buf = (uint8_t *)malloc(FRAME_LEN);
    if (!buf) { fprintf(stderr, "FAIL: oom\n"); return 1; }

    int failures = 0;
    for (int f = 0; f < FRAMES; f++) {
        for (size_t i = 0; i < FRAME_LEN; i++) {
            buf[i] = (uint8_t)((i * 131u) + (unsigned)f * 17u);
        }

        rc = punktfunk_host_submit_frame(host, buf, FRAME_LEN, (uint64_t)f * 1000000u, 0);
        if (rc != PUNKTFUNK_STATUS_OK) {
            fprintf(stderr, "FAIL: submit frame %d rc=%d\n", f, (int)rc);
            failures++;
            continue;
        }

        PunktfunkFrame out;
        memset(&out, 0, sizeof(out));
        rc = punktfunk_client_poll_frame(client, &out);
        if (rc != PUNKTFUNK_STATUS_OK) {
            fprintf(stderr, "FAIL: poll frame %d rc=%d (expected recovery)\n", f, (int)rc);
            failures++;
            continue;
        }
        if (out.len != FRAME_LEN || memcmp(out.data, buf, FRAME_LEN) != 0) {
            fprintf(stderr, "FAIL: frame %d mismatch (len=%zu want=%zu)\n",
                    f, (size_t)out.len, FRAME_LEN);
            failures++;
            continue;
        }
        if (out.frame_index != (uint32_t)f) {
            fprintf(stderr, "FAIL: frame %d wrong index %u\n", f, out.frame_index);
            failures++;
        }
    }

    PunktfunkStats st;
    memset(&st, 0, sizeof(st));
    punktfunk_get_stats(client, &st);
    printf("client stats: completed=%llu recovered_shards=%llu dropped_pkts=%llu rx_pkts=%llu\n",
           (unsigned long long)st.frames_completed,
           (unsigned long long)st.fec_recovered_shards,
           (unsigned long long)st.packets_dropped,
           (unsigned long long)st.packets_received);

    if (st.fec_recovered_shards == 0) {
        fprintf(stderr, "FAIL: expected FEC to recover lost shards, but recovered 0\n");
        failures++;
    }

    free(buf);
    punktfunk_session_free(host);
    punktfunk_session_free(client);

    if (failures == 0) {
        printf("PASS: %d frames round-tripped byte-exact through lossy loopback\n", FRAMES);
        return 0;
    }
    fprintf(stderr, "FAILED with %d errors\n", failures);
    return 1;
}
