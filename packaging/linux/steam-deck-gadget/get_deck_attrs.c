// Query a physical Steam Deck's feature reports (SET command then GET response) over hidraw to get
// the FULL blobs (usbmon truncates to 32 bytes). Steam feature reports are unnumbered 64-byte; the
// hidraw buffer prefixes a report-id byte (0).
#include <linux/hidraw.h>
#include <sys/ioctl.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
static void dump(const char *name, unsigned char *b, int n) {
    printf("%s rc=%d:", name, n);
    for (int i = 0; i < (n > 0 ? n : 0); i++) printf(" %02x", b[i]);
    printf("\n");
}
int main(int argc, char **argv) {
    if (argc < 2) { printf("usage: %s /dev/hidrawN\n", argv[0]); return 1; }
    int fd = open(argv[1], O_RDWR);
    if (fd < 0) { perror("open"); return 1; }
    // SET [reportid=0, cmd, len, attr, ...] then GET the response.
    unsigned char queries[][4] = {
        {0x83, 0x00, 0x00, 0x00}, // GET_ATTRIBUTES_VALUES
        {0xae, 0x16, 0x01, 0x00}, // GET_STRING_ATTRIBUTE, serial (attr 1)
        {0xae, 0x16, 0x00, 0x00}, // GET_STRING_ATTRIBUTE, board serial (attr 0)
        {0xae, 0x16, 0x02, 0x00}, // attr 2 = ATTRIB_STR_COUNT, past the last string attribute
    };
    for (int q = 0; q < 4; q++) {
        unsigned char set[65] = {0};
        set[0] = 0; // report id 0
        memcpy(set + 1, queries[q], 4);
        int sr = ioctl(fd, HIDIOCSFEATURE(65), set);
        usleep(3000);
        unsigned char get[65] = {0};
        get[0] = 0;
        int gr = ioctl(fd, HIDIOCGFEATURE(65), get);
        printf("=== query cmd=%02x attr=%02x  (SET rc=%d) ===\n", queries[q][0], queries[q][2], sr);
        dump("  GET", get, gr);
    }
    return 0;
}
