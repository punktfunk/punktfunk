// What deck_raw_gadget answers on ep0 for each control request, driven through a fake ioctl
// that records the write, read, stall or configure it asks raw_gadget for. Linux only (the
// gadget reads linux/usb/ch9.h); no device, no root:
//   gcc -O2 -o deck_raw_gadget_test deck_raw_gadget_test.c -lpthread && ./deck_raw_gadget_test
#include <assert.h>
#include <stdarg.h>
#define main deck_main
#define ioctl fake_ioctl
#include "deck_raw_gadget.c"
#undef main
#undef ioctl

/* What the last control request did on ep0. */
static struct { int wrote; int len; __u8 data[4096]; int reads; int read_len; int stalls; int configures; int vbus; } ep0;
/* The data stage a SET_REPORT carries. */
static __u8 host_data[64]; static int host_len;

int fake_ioctl(int fd_, unsigned long req, ...){
    (void)fd_;
    va_list ap; va_start(ap, req);
    int r = -1;
    if(req == USB_RAW_IOCTL_EP0_WRITE){
        struct usb_raw_ep_io *io = va_arg(ap, void*);
        ep0.wrote = 1; ep0.len = io->length; memcpy(ep0.data, io->data, io->length); r = io->length;
    } else if(req == USB_RAW_IOCTL_EP0_READ){
        struct usb_raw_ep_io *io = va_arg(ap, void*);
        ep0.reads++; ep0.read_len = io->length;
        r = (int)io->length < host_len ? (int)io->length : host_len; memcpy(io->data, host_data, r);
    } else if(req == USB_RAW_IOCTL_EP0_STALL){ ep0.stalls++; r = 0; }
    else if(req == USB_RAW_IOCTL_CONFIGURE){ ep0.configures++; r = 0; }
    else if(req == USB_RAW_IOCTL_VBUS_DRAW){ (void)va_arg(ap, unsigned); ep0.vbus++; r = 0; }
    else if(req == USB_RAW_IOCTL_EP_ENABLE){ r = 7; } // an endpoint handle
    va_end(ap);
    return r;
}

static void ctrl_req(__u8 rt, __u8 req, __u16 v, __u16 i, __u16 l){
    memset(&ep0, 0, sizeof ep0);
    struct usb_ctrlrequest c = { .bRequestType = rt, .bRequest = req, .wValue = v, .wIndex = i, .wLength = l };
    handle_control(&c);
}
#define WROTE(n) (ep0.wrote && ep0.len == (int)(n))
#define ACKED() (ep0.reads == 1 && ep0.read_len == 0 && !ep0.wrote && ep0.stalls == 0)

int main(void){
    dbg = 0; build_config();

    // GET_DESCRIPTOR. The device and HID descriptors go out whole, whatever wLength asks
    // (the config, string and report descriptors are clamped to it).
    ctrl_req(0x80, USB_REQ_GET_DESCRIPTOR, USB_DT_DEVICE<<8, 0, 8);
    assert(WROTE(18) && ep0.data[8] == 0xde && ep0.data[9] == 0x28);
    ctrl_req(0x80, USB_REQ_GET_DESCRIPTOR, USB_DT_CONFIG<<8, 0, 9);
    assert(WROTE(9) && ep0.data[2] == sizeof(cfg) && ep0.data[4] == 3);
    ctrl_req(0x80, USB_REQ_GET_DESCRIPTOR, USB_DT_CONFIG<<8, 0, 255);
    assert(WROTE(sizeof(cfg)));
    ctrl_req(0x80, USB_REQ_GET_DESCRIPTOR, USB_DT_STRING<<8 | 0, 0, 255);
    assert(WROTE(4) && ep0.data[2] == 0x09 && ep0.data[3] == 0x04);
    ctrl_req(0x80, USB_REQ_GET_DESCRIPTOR, USB_DT_STRING<<8 | 2, 0x0409, 255);
    assert(WROTE(2 + 21 * 2) && ep0.data[2] == 'S' && ep0.data[3] == 0);
    ctrl_req(0x80, USB_REQ_GET_DESCRIPTOR, USB_DT_STRING<<8 | 3, 0x0409, 6);
    assert(WROTE(6));
    ctrl_req(0x80, USB_REQ_GET_DESCRIPTOR, USB_DT_STRING<<8 | 9, 0x0409, 255);
    assert(WROTE(2)); // an index the Deck does not have: an empty string
    ctrl_req(0x81, USB_REQ_GET_DESCRIPTOR, HID_RPT_DT<<8, 0, 255);
    assert(WROTE(sizeof RDESC_MOUSE) && ep0.data[3] == 0x02);
    ctrl_req(0x81, USB_REQ_GET_DESCRIPTOR, HID_RPT_DT<<8, 1, 255);
    assert(WROTE(sizeof RDESC_KBD) && ep0.data[3] == 0x06);
    ctrl_req(0x81, USB_REQ_GET_DESCRIPTOR, HID_RPT_DT<<8, 2, 255);
    assert(WROTE(sizeof RDESC_CTRL) && ep0.data[0] == 0x06);
    ctrl_req(0x81, USB_REQ_GET_DESCRIPTOR, HID_RPT_DT<<8, 7, 255);
    assert(WROTE(sizeof RDESC_CTRL)); // any other interface answers as the controller
    ctrl_req(0x81, USB_REQ_GET_DESCRIPTOR, HID_RPT_DT<<8, 2, 10);
    assert(WROTE(10));
    ctrl_req(0x81, USB_REQ_GET_DESCRIPTOR, HID_DT<<8, 2, 255);
    assert(WROTE(9) && ep0.data[4] == 33); // the controller's country code
    ctrl_req(0x81, USB_REQ_GET_DESCRIPTOR, HID_DT<<8, 0, 4);
    assert(WROTE(9) && ep0.data[4] == 0);
    ctrl_req(0x80, USB_REQ_GET_DESCRIPTOR, USB_DT_DEVICE_QUALIFIER<<8, 0, 10);
    assert(ep0.stalls == 1 && !ep0.wrote);

    // The other standard requests.
    ctrl_req(0x00, USB_REQ_SET_CONFIGURATION, 1, 0, 0);
    assert(ep0.configures == 1 && ep0.vbus == 1 && ACKED() && configured == 1 && ctrl_ep == 7);
    ctrl_req(0x01, USB_REQ_SET_INTERFACE, 0, 2, 0);
    assert(ACKED());
    ctrl_req(0x80, USB_REQ_GET_STATUS, 0, 0, 2);
    assert(WROTE(2) && ep0.data[0] == 0 && ep0.data[1] == 0);
    ctrl_req(0x00, USB_REQ_SET_FEATURE, 1, 0, 0);
    assert(ep0.stalls == 1);

    // HID class requests. GET_REPORT is the serial blob, echoing the last SET_REPORT command.
    ctrl_req(0xa1, 0x01, 0x0300, 2, 64);
    assert(WROTE(64) && ep0.data[0] == 0xAE && ep0.data[1] == 10 && ep0.data[2] == 1
        && memcmp(ep0.data + 3, "PFDECK0001", 10) == 0 && ep0.data[13] == 0);
    ctrl_req(0xa1, 0x01, 0x0300, 2, 16);
    assert(WROTE(16));
    ctrl_req(0xa1, 0x01, 0x0300, 2, 200);
    assert(WROTE(64));
    host_len = 3; host_data[0] = 0x5e; host_data[1] = 0; host_data[2] = 0;
    ctrl_req(0x21, 0x09, 0x0300, 2, 3);
    assert(ep0.reads == 1 && ep0.read_len == 3 && last_feature_cmd == 0x5e && !ep0.wrote && ep0.stalls == 0);
    ctrl_req(0x21, 0x09, 0x0300, 2, 100);
    assert(ep0.read_len == 64);
    ctrl_req(0xa1, 0x01, 0x0300, 2, 64);
    assert(WROTE(64) && ep0.data[0] == 0x5e);
    host_len = 0;
    ctrl_req(0x21, 0x09, 0x0300, 2, 3);
    assert(last_feature_cmd == 0x5e); // an empty data stage keeps the last command
    ctrl_req(0x21, 0x0a, 0, 2, 0);
    assert(ACKED()); // SET_IDLE
    ctrl_req(0x21, 0x0b, 0, 1, 0);
    assert(ACKED()); // SET_PROTOCOL
    ctrl_req(0xa1, 0x03, 0, 1, 1);
    assert(WROTE(1) && ep0.data[0] == 0); // GET_PROTOCOL
    ctrl_req(0xa1, 0x02, 0, 1, 1);
    assert(ep0.stalls == 1);

    // Vendor requests: stalled.
    ctrl_req(0xc0, 0x01, 0, 0, 1);
    assert(ep0.stalls == 1 && !ep0.wrote);

    puts("deck_raw_gadget_test: ok");
    return 0;
}
