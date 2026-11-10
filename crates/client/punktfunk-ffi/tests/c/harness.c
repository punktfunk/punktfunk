/*
 * punktfunk C ABI harness — M1 acceptance.
 *
 * Proves the C ABI links from C and round-trips encoded access units through the full
 * packetize -> FEC -> in-process loopback (with deterministic packet loss) -> FEC
 * recover -> reassemble path, recovering every byte exactly.
 *
 * Build/run: see tests/c/run.sh (also driven by `cargo test --test c_abi`).
 */
#include "punktfunk_core.h"

#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static PunktfunkConfig make_config(uint32_t role, uint32_t drop_period) {
    PunktfunkConfig c;
    memset(&c, 0, sizeof(c));
    c.struct_size = (uint32_t)sizeof(PunktfunkConfig);
    c.role = role;                 /* 0 = host, 1 = client */
    c.fec_scheme = 0;              /* GF(2^8) */
    c.fec_percent = 25;
    c.max_data_per_block = 64;
    c.shard_payload = 1024;
    c.max_frame_bytes = 8 * 1024 * 1024;
    c.loopback_drop_period = drop_period;
    return c;
}

int main(void) {
    printf("punktfunk C ABI harness (abi_version=%u)\n", punktfunk_abi_version());

    /* PunktfunkConnectOpts: the C compiler agrees with Rust's const-asserted layout, 200 bytes on
     * 64-bit and 144 on 32-bit with no tail padding, so an appended field never lands in bytes an
     * older caller's sizeof covered. The size-prefix guard rejects an undersized struct as a
     * status, not a read. The staticlib this links always carries quic. */
#ifdef PUNKTFUNK_FEATURE_QUIC
    if (sizeof(PunktfunkConnectOpts) != (sizeof(void *) == 8 ? 200u : 144u)
        || offsetof(PunktfunkConnectOpts, video_fit) != (sizeof(void *) == 8 ? 100u : 72u)
        || offsetof(PunktfunkConnectOpts, delivery_flags) != (sizeof(void *) == 8 ? 121u : 85u)
        || offsetof(PunktfunkConnectOpts, profile_id) != (sizeof(void *) == 8 ? 128u : 92u)
        || offsetof(PunktfunkConnectOpts, pyrowave_bpp_x100) != (sizeof(void *) == 8 ? 136u : 96u)
        || offsetof(PunktfunkConnectOpts, on_pending) != (sizeof(void *) == 8 ? 144u : 104u)
        || offsetof(PunktfunkConnectOpts, screens) != (sizeof(void *) == 8 ? 160u : 112u)
        || offsetof(PunktfunkConnectOpts, unlock) != (sizeof(void *) == 8 ? 176u : 124u)
        || offsetof(PunktfunkConnectOpts, prompts_unlock) != (sizeof(void *) == 8 ? 184u : 128u)
        || offsetof(PunktfunkConnectOpts, view_width) != (sizeof(void *) == 8 ? 192u : 136u)
        || sizeof(PunktfunkProfileUnlock) != (sizeof(void *) == 8 ? 40u : 24u)
        || sizeof(PunktfunkScreen) != 24u) {
        fprintf(stderr, "FAIL: PunktfunkConnectOpts is %zu bytes, video_fit at %zu, delivery_flags at %zu, profile_id at %zu, pyrowave_bpp_x100 at %zu, on_pending at %zu, screens at %zu, unlock at %zu, view_width at %zu; PunktfunkScreen is %zu, PunktfunkProfileUnlock %zu\n",
                sizeof(PunktfunkConnectOpts), offsetof(PunktfunkConnectOpts, video_fit),
                offsetof(PunktfunkConnectOpts, delivery_flags),
                offsetof(PunktfunkConnectOpts, profile_id),
                offsetof(PunktfunkConnectOpts, pyrowave_bpp_x100),
                offsetof(PunktfunkConnectOpts, on_pending),
                offsetof(PunktfunkConnectOpts, screens), offsetof(PunktfunkConnectOpts, unlock),
                offsetof(PunktfunkConnectOpts, view_width),
                sizeof(PunktfunkScreen), sizeof(PunktfunkProfileUnlock));
        return 1;
    }
    /* A null connection is a status for the mode-and-view ask too. */
    if (punktfunk_connection_request_mode_view(NULL, 2048, 1152, 60, 1366, 768)
        != PUNKTFUNK_STATUS_NULL_POINTER) {
        fprintf(stderr, "FAIL: punktfunk_connection_request_mode_view on a null connection\n");
        return 1;
    }
    {
        /* The unlock message builds from C and refuses an uppercase seat id. */
        uint8_t challenge[32] = {0}, fp[32] = {0}, msg[PUNKTFUNK_UNLOCK_MESSAGE_LEN];
        if (punktfunk_profile_unlock_message(challenge, "0123456789abcdef0123456789abcdef", fp, fp, msg)
                != PUNKTFUNK_STATUS_OK
            || punktfunk_profile_unlock_message(challenge, "0123456789ABCDEF0123456789abcdef", fp, fp, msg)
                != PUNKTFUNK_STATUS_INVALID_ARG) {
            fprintf(stderr, "FAIL: punktfunk_profile_unlock_message\n");
            return 1;
        }
    }
    {
        /* The screen calls link from C; a null connection is a status and writes nothing. */
        PunktfunkScreen screens[PUNKTFUNK_SCREENS_MAX];
        PunktfunkFrame frame;
        uintptr_t count = 7;
        uint32_t generation = 7;
        memset(screens, 0, sizeof(screens));
        if (punktfunk_connection_screen_layout(NULL, screens, PUNKTFUNK_SCREENS_MAX, &count,
                                               &generation) != PUNKTFUNK_STATUS_NULL_POINTER
            || punktfunk_connection_request_screens(NULL, screens, 2) != PUNKTFUNK_STATUS_NULL_POINTER
            || punktfunk_connection_request_screen_keyframe(NULL, 1) != PUNKTFUNK_STATUS_NULL_POINTER
            || punktfunk_connection_next_screen_au(NULL, 1, &frame, 0) != PUNKTFUNK_STATUS_NULL_POINTER
            || count != 7 || generation != 7) {
            fprintf(stderr, "FAIL: screen calls on a null connection\n");
            return 1;
        }
    }
    /* The PyroWave quality row's rate and warning: 4K120 at 1.6 bits per pixel on an unknown link. */
    {
        char line[160];
        uint32_t kbps = punktfunk_pyrowave_kbps(3840, 2160, 120, false, 8, 160);
        if (kbps != 1592524u
            || punktfunk_pyrowave_link_warning(kbps, PUNKTFUNK_IFACE_KIND_UNKNOWN, 0, line, sizeof(line))
                   != PUNKTFUNK_STATUS_OK
            || strstr(line, "1.6 Gbit/s") == NULL) {
            fprintf(stderr, "FAIL: PyroWave quality priced %u kbps, warned \"%s\"\n", (unsigned)kbps, line);
            return 1;
        }
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
    {
        /* The recovery reports link from C; a null connection is a status and writes nothing. */
        bool admit = true, hold = true;
        uint32_t gap = 7;
        if (punktfunk_connection_recovery_on_frame(NULL, 1, true, 0, &admit, &gap)
                != PUNKTFUNK_STATUS_NULL_POINTER
            || punktfunk_connection_recovery_on_decoded(
                   NULL, 0, false, PUNKTFUNK_ANCHOR_EVIDENCE_UNAVAILABLE, 0, &hold, NULL)
                != PUNKTFUNK_STATUS_NULL_POINTER
            || punktfunk_connection_recovery_on_no_output(NULL) != PUNKTFUNK_STATUS_NULL_POINTER
            || punktfunk_connection_recovery_lost(NULL) != PUNKTFUNK_STATUS_NULL_POINTER
            || punktfunk_connection_recovery_reset(NULL) != PUNKTFUNK_STATUS_NULL_POINTER
            || !admit || !hold || gap != 7) {
            fprintf(stderr, "FAIL: recovery reports on a null connection\n");
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

    {
        /* A 320x180 Main-profile sequence-header OBU parses; a cut one does not. */
        static const uint8_t seq[] = {0x0a, 0x0b, 0x00, 0x00, 0x00, 0x04, 0x3c,
                                      0xfe, 0xcc, 0x4a, 0xf9, 0x00, 0x40};
        PunktfunkAv1SequenceInfo info;
        PunktfunkStatus s1 = punktfunk_av1_sequence_info(seq, sizeof seq, &info);
        PunktfunkStatus s2 = punktfunk_av1_sequence_info(seq, sizeof seq - 1, &info);
        if (s1 != PUNKTFUNK_STATUS_OK || info.max_width != 320 || info.max_height != 180
            || s2 != PUNKTFUNK_STATUS_INVALID_ARG) {
            fprintf(stderr, "FAIL: av1 sequence info (st=%d/%d)\n", (int)s1, (int)s2);
            return 1;
        }
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
