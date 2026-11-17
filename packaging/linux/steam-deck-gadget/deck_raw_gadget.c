// raw_gadget emulator of a real 3-interface USB Steam Deck (28DE:1205): mouse=iface0, keyboard=iface1,
// controller=iface2 (the structure Steam filters for). Unlike f_hid, raw_gadget lets us answer EVERY
// control transfer — including the HID feature reports hid-steam/Steam need (the serial etc.) — so the
// Deck fully initialises (gamepad evdev) and Steam can read controller details (no "zombie").
//
// Descriptors captured verbatim from a physical Deck. Build (static, to run on SteamOS):
//   gcc -O2 -static -o deck_raw_gadget deck_raw_gadget.c -lpthread
// Run as root on a host with dummy_hcd loaded:  ./deck_raw_gadget [seconds]
#include <linux/usb/ch9.h>
#include <sys/ioctl.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

/* ---- raw_gadget UAPI (inlined so we don't depend on the header) ---- */
#define UDC_NAME_LENGTH_MAX 128
struct usb_raw_init { __u8 driver_name[UDC_NAME_LENGTH_MAX]; __u8 device_name[UDC_NAME_LENGTH_MAX]; __u8 speed; };
enum usb_raw_event_type { USB_RAW_EVENT_INVALID, USB_RAW_EVENT_CONNECT, USB_RAW_EVENT_CONTROL };
struct usb_raw_event { __u32 type; __u32 length; __u8 data[0]; };
struct usb_raw_ep_io { __u16 ep; __u16 flags; __u32 length; __u8 data[0]; };
#define USB_RAW_EPS_NUM_MAX 30
#define USB_RAW_EP_NAME_MAX 16
struct usb_raw_ep_caps { __u32 type_control:1, type_iso:1, type_bulk:1, type_int:1, dir_in:1, dir_out:1; };
struct usb_raw_ep_limits { __u16 maxpacket_limit; __u16 max_streams; __u32 reserved; };
struct usb_raw_ep_info { __u8 name[USB_RAW_EP_NAME_MAX]; __u32 addr; struct usb_raw_ep_caps caps; struct usb_raw_ep_limits limits; };
struct usb_raw_eps_info { struct usb_raw_ep_info eps[USB_RAW_EPS_NUM_MAX]; };
#define USB_RAW_IOCTL_INIT        _IOW('U', 0, struct usb_raw_init)
#define USB_RAW_IOCTL_RUN         _IO('U', 1)
#define USB_RAW_IOCTL_EVENT_FETCH _IOR('U', 2, struct usb_raw_event)
#define USB_RAW_IOCTL_EP0_WRITE   _IOW('U', 3, struct usb_raw_ep_io)
#define USB_RAW_IOCTL_EP0_READ    _IOWR('U', 4, struct usb_raw_ep_io)
#define USB_RAW_IOCTL_EP_ENABLE   _IOW('U', 5, struct usb_endpoint_descriptor)
#define USB_RAW_IOCTL_EP_WRITE    _IOW('U', 7, struct usb_raw_ep_io)
#define USB_RAW_IOCTL_CONFIGURE   _IO('U', 9)
#define USB_RAW_IOCTL_VBUS_DRAW   _IOW('U', 10, __u32)
#define USB_RAW_IOCTL_EPS_INFO    _IOR('U', 11, struct usb_raw_eps_info)
#define USB_RAW_IOCTL_EP0_STALL   _IO('U', 12)

/* ---- captured-from-hardware report descriptors ---- */
static const __u8 RDESC_MOUSE[] = {
    0x05,0x01,0x09,0x02,0xa1,0x01,0x09,0x01,0xa1,0x00,0x05,0x09,0x19,0x01,0x29,0x02,
    0x15,0x00,0x25,0x01,0x75,0x01,0x95,0x02,0x81,0x02,0x75,0x06,0x95,0x01,0x81,0x01,
    0x05,0x01,0x09,0x30,0x09,0x31,0x15,0x81,0x25,0x7f,0x75,0x08,0x95,0x02,0x81,0x06,
    0x95,0x01,0x09,0x38,0x81,0x06,0x05,0x0c,0x0a,0x38,0x02,0x95,0x01,0x81,0x06,0xc0,0xc0 };
static const __u8 RDESC_KBD[] = {
    0x05,0x01,0x09,0x06,0xa1,0x01,0x05,0x07,0x19,0xe0,0x29,0xe7,0x15,0x00,0x25,0x01,
    0x75,0x01,0x95,0x08,0x81,0x02,0x81,0x01,0x19,0x00,0x29,0x65,0x15,0x00,0x25,0x65,
    0x75,0x08,0x95,0x06,0x81,0x00,0xc0 };
static const __u8 RDESC_CTRL[] = { // the real Deck controller, interface 2
    0x06,0xff,0xff,0x09,0x01,0xa1,0x01,0x09,0x02,0x09,0x03,0x15,0x00,0x26,0xff,0x00,
    0x75,0x08,0x95,0x40,0x81,0x02,0x09,0x06,0x09,0x07,0x15,0x00,0x26,0xff,0x00,0x75,
    0x08,0x95,0x40,0xb1,0x02,0xc0 };

/* ---- HID descriptor (one per interface, points at the report descriptor length) ---- */
struct hid_desc { __u8 bLength,bDescriptorType; __u16 bcdHID; __u8 bCountryCode,bNumDescriptors,bReportType; __u16 wReportLength; } __attribute__((packed));
/* Exact 7-byte endpoint descriptor — `struct usb_endpoint_descriptor` is 9 bytes (audio bRefresh/
   bSynchAddress), which would inject 2 garbage bytes per endpoint into the wire config + mis-parse. */
struct ep_desc7 { __u8 bLength,bDescriptorType,bEndpointAddress,bmAttributes; __u16 wMaxPacketSize; __u8 bInterval; } __attribute__((packed));

/* ---- full config descriptor, assembled to mirror the real Deck (3 HID interfaces) ---- */
struct config_blob {
    struct usb_config_descriptor config;
    struct usb_interface_descriptor i0; struct hid_desc h0; struct ep_desc7 e0;
    struct usb_interface_descriptor i1; struct hid_desc h1; struct ep_desc7 e1;
    struct usb_interface_descriptor i2; struct hid_desc h2; struct ep_desc7 e2;
} __attribute__((packed));
/* Full 9-byte endpoint descriptors, used only for the EP_ENABLE ioctl. */
static struct usb_endpoint_descriptor epfull0, epfull1, epfull2;

static struct usb_device_descriptor dev_desc = {
    .bLength = USB_DT_DEVICE_SIZE, .bDescriptorType = USB_DT_DEVICE, .bcdUSB = 0x0200,
    .bDeviceClass = 0, .bDeviceSubClass = 0, .bDeviceProtocol = 0, .bMaxPacketSize0 = 64,
    .idVendor = 0x28de, .idProduct = 0x1205, .bcdDevice = 0x0300,
    .iManufacturer = 1, .iProduct = 2, .iSerialNumber = 3, .bNumConfigurations = 1 };

#define HID_DT 0x21
#define HID_RPT_DT 0x22
static struct config_blob cfg;
static void build_config(void) {
    memset(&cfg, 0, sizeof(cfg));
    cfg.config = (struct usb_config_descriptor){ .bLength = USB_DT_CONFIG_SIZE, .bDescriptorType = USB_DT_CONFIG,
        .wTotalLength = sizeof(cfg), .bNumInterfaces = 3, .bConfigurationValue = 1, .iConfiguration = 0,
        .bmAttributes = 0x80, .bMaxPower = 250 };
    // iface 0: mouse (subclass 0, protocol 2), EP 0x81 IN 8
    cfg.i0 = (struct usb_interface_descriptor){ .bLength = USB_DT_INTERFACE_SIZE, .bDescriptorType = USB_DT_INTERFACE,
        .bInterfaceNumber = 0, .bNumEndpoints = 1, .bInterfaceClass = 3, .bInterfaceSubClass = 0, .bInterfaceProtocol = 2 };
    cfg.h0 = (struct hid_desc){ sizeof(struct hid_desc), HID_DT, 0x0110, 0, 1, HID_RPT_DT, sizeof(RDESC_MOUSE) };
    cfg.e0 = (struct ep_desc7){ .bLength = USB_DT_ENDPOINT_SIZE, .bDescriptorType = USB_DT_ENDPOINT,
        .bEndpointAddress = 0x81, .bmAttributes = USB_ENDPOINT_XFER_INT, .wMaxPacketSize = 8, .bInterval = 4 };
    // iface 1: keyboard (subclass 1 boot, protocol 1), EP 0x82 IN 8
    cfg.i1 = (struct usb_interface_descriptor){ .bLength = USB_DT_INTERFACE_SIZE, .bDescriptorType = USB_DT_INTERFACE,
        .bInterfaceNumber = 1, .bNumEndpoints = 1, .bInterfaceClass = 3, .bInterfaceSubClass = 1, .bInterfaceProtocol = 1 };
    cfg.h1 = (struct hid_desc){ sizeof(struct hid_desc), HID_DT, 0x0110, 0, 1, HID_RPT_DT, sizeof(RDESC_KBD) };
    cfg.e1 = (struct ep_desc7){ .bLength = USB_DT_ENDPOINT_SIZE, .bDescriptorType = USB_DT_ENDPOINT,
        .bEndpointAddress = 0x82, .bmAttributes = USB_ENDPOINT_XFER_INT, .wMaxPacketSize = 8, .bInterval = 4 };
    // iface 2: the controller (subclass 0, protocol 0), EP 0x83 IN 64
    cfg.i2 = (struct usb_interface_descriptor){ .bLength = USB_DT_INTERFACE_SIZE, .bDescriptorType = USB_DT_INTERFACE,
        .bInterfaceNumber = 2, .bNumEndpoints = 1, .bInterfaceClass = 3, .bInterfaceSubClass = 0, .bInterfaceProtocol = 0 };
    cfg.h2 = (struct hid_desc){ sizeof(struct hid_desc), HID_DT, 0x0110, 33, 1, HID_RPT_DT, sizeof(RDESC_CTRL) };
    cfg.e2 = (struct ep_desc7){ .bLength = USB_DT_ENDPOINT_SIZE, .bDescriptorType = USB_DT_ENDPOINT,
        .bEndpointAddress = 0x83, .bmAttributes = USB_ENDPOINT_XFER_INT, .wMaxPacketSize = 64, .bInterval = 4 };
    // Full 9-byte endpoint descriptors for EP_ENABLE (the ioctl wants struct usb_endpoint_descriptor).
    #define MKFULL(F,E) do{ memset(&F,0,sizeof F); F.bLength=USB_DT_ENDPOINT_SIZE; F.bDescriptorType=USB_DT_ENDPOINT; \
        F.bEndpointAddress=E.bEndpointAddress; F.bmAttributes=E.bmAttributes; F.wMaxPacketSize=E.wMaxPacketSize; F.bInterval=E.bInterval; }while(0)
    MKFULL(epfull0, cfg.e0); MKFULL(epfull1, cfg.e1); MKFULL(epfull2, cfg.e2);
}

static int fd = -1;
static int ctrl_ep = -1; // raw handle for the controller IN endpoint
static volatile int running = 1;
static volatile int configured = 0;
static int do_stream = 1; // argv: "nostream" disables the input streamer
static int dbg = 1;
static __u8 last_feature_cmd = 0; // command byte of the last SET_REPORT, whichever interface

static void log_line(const char *s){ fprintf(stderr, "%s\n", s); }

static int ep0_write(const void *data, int len){
    char buf[sizeof(struct usb_raw_ep_io)+4096]; struct usb_raw_ep_io *io=(void*)buf;
    io->ep=0; io->flags=0; io->length=len; if(len) memcpy(io->data,data,len);
    int r=ioctl(fd, USB_RAW_IOCTL_EP0_WRITE, io);
    if(r<0){ char m[80]; snprintf(m,sizeof m,"  !! ep0_write(len=%d) errno=%d", len, errno); log_line(m); }
    return r;
}
static int ep0_read(void *data, int len){
    char buf[sizeof(struct usb_raw_ep_io)+4096]; struct usb_raw_ep_io *io=(void*)buf;
    io->ep=0; io->flags=0; io->length=len;
    int r=ioctl(fd, USB_RAW_IOCTL_EP0_READ, io); if(r>=0 && data) memcpy(data, io->data, r<len?r:len); return r;
}
static void ep0_stall(void){ ioctl(fd, USB_RAW_IOCTL_EP0_STALL); }
// Complete a no-data OUT control transfer: the status stage is an IN handled by a zero-length READ.
static void ep0_ack(void){ ep0_read(NULL,0); }

// String descriptors.
static int build_string(int idx, __u8 *out){
    if(idx==0){ out[0]=4; out[1]=USB_DT_STRING; out[2]=0x09; out[3]=0x04; return 4; }
    const char *s = idx==1?"Valve Software":idx==2?"Steam Deck Controller":idx==3?"PFDECK0001":"";
    int n=strlen(s); out[0]=2+n*2; out[1]=USB_DT_STRING; for(int i=0;i<n;i++){ out[2+i*2]=s[i]; out[3+i*2]=0; } return 2+n*2;
}

static void enable_endpoints(void){
    // Enable the 3 interrupt-IN endpoints; remember the controller's handle for streaming.
    int e0=errno; int h0=ioctl(fd, USB_RAW_IOCTL_EP_ENABLE, &epfull0); e0=errno;
    int h1=ioctl(fd, USB_RAW_IOCTL_EP_ENABLE, &epfull1); int e1=errno;
    int h2=ioctl(fd, USB_RAW_IOCTL_EP_ENABLE, &epfull2); int e2=errno;
    ctrl_ep = h2;
    char m[128]; snprintf(m,sizeof m,"endpoints enabled: mouse=%d(e%d) kbd=%d(e%d) ctrl=%d(e%d)", h0,h0<0?e0:0,h1,h1<0?e1:0,h2,h2<0?e2:0); log_line(m);
}

enum ep0_action { EP0_STALL, EP0_ACK, EP0_WRITE, EP0_SET_CONFIG, EP0_SET_REPORT };
/* One control request's answer, decided before any ep0 I/O: `data`/`len` for EP0_WRITE, the data
   stage to read for EP0_SET_REPORT. */
struct ep0_plan { enum ep0_action action; const void *data; int len; };

static struct ep0_plan ep0_plan_write(const void *data, int len){ return (struct ep0_plan){ EP0_WRITE, data, len }; }
static struct ep0_plan ep0_plan_act(enum ep0_action a){ return (struct ep0_plan){ a, NULL, 0 }; }

/* GET_DESCRIPTOR. The config, string and report descriptors are cut to wLength; the device and
   HID descriptors go out whole. `scratch` (260 bytes) backs the string reply. */
static struct ep0_plan plan_descriptor(const struct usb_ctrlrequest *ctrl, __u8 *scratch){
    int type = ctrl->wValue >> 8, di = ctrl->wValue & 0xff, idx = ctrl->wIndex & 0xff;
    int l;
    switch(type){
    case USB_DT_DEVICE: return ep0_plan_write(&dev_desc, dev_desc.bLength);
    case USB_DT_CONFIG: l=sizeof(cfg); if(l>ctrl->wLength) l=ctrl->wLength; return ep0_plan_write(&cfg, l);
    case USB_DT_STRING: l=build_string(di,scratch); if(l>ctrl->wLength) l=ctrl->wLength; return ep0_plan_write(scratch, l);
    case HID_RPT_DT: { // the interface in wIndex; any other index answers as the controller
        const __u8 *r = idx==0?RDESC_MOUSE:idx==1?RDESC_KBD:RDESC_CTRL;
        l = idx==0?sizeof(RDESC_MOUSE):idx==1?sizeof(RDESC_KBD):sizeof(RDESC_CTRL);
        if(l>ctrl->wLength) l=ctrl->wLength;
        return ep0_plan_write(r, l);
    }
    case HID_DT: { const struct hid_desc *h = idx==0?&cfg.h0:idx==1?&cfg.h1:&cfg.h2; return ep0_plan_write(h, h->bLength); }
    default: return ep0_plan_act(EP0_STALL);
    }
}

/* Standard requests. `scratch` also carries the 2-byte GET_STATUS reply. */
static struct ep0_plan plan_standard(const struct usb_ctrlrequest *ctrl, __u8 *scratch){
    switch(ctrl->bRequest){
    case USB_REQ_GET_DESCRIPTOR: return plan_descriptor(ctrl, scratch);
    case USB_REQ_SET_CONFIGURATION: return ep0_plan_act(EP0_SET_CONFIG);
    case USB_REQ_SET_INTERFACE: return ep0_plan_act(EP0_ACK);
    case USB_REQ_GET_STATUS: scratch[0]=0; scratch[1]=0; return ep0_plan_write(scratch, 2);
    default: return ep0_plan_act(EP0_STALL);
    }
}

/* HID class requests. GET_REPORT is the serial-style feature blob for the controller (harmless
   for the others): [cmd, len, 0x01, serial...], `cmd` echoing the last SET_REPORT, 0xAE before
   any. `scratch` (64 bytes) carries it. */
static struct ep0_plan plan_class(const struct usb_ctrlrequest *ctrl, __u8 last_cmd, __u8 *scratch){
    int l = ctrl->wLength>64?64:ctrl->wLength;
    switch(ctrl->bRequest){
    case 0x01: { // GET_REPORT
        const char *serial = "PFDECK0001";
        memset(scratch,0,64);
        scratch[0]=last_cmd?last_cmd:0xAE; scratch[1]=strlen(serial); scratch[2]=0x01;
        memcpy(scratch+3, serial, strlen(serial));
        return ep0_plan_write(scratch, l);
    }
    case 0x09: return (struct ep0_plan){ EP0_SET_REPORT, NULL, l }; // SET_REPORT: read the data stage
    case 0x0a: return ep0_plan_act(EP0_ACK); // SET_IDLE (OUT/no-data)
    case 0x0b: return ep0_plan_act(EP0_ACK); // SET_PROTOCOL (OUT/no-data)
    case 0x03: scratch[0]=0; return ep0_plan_write(scratch, 1); // GET_PROTOCOL
    default: return ep0_plan_act(EP0_STALL);
    }
}

/* What one control request gets, by request type. Pure: touches no fd. */
static struct ep0_plan plan_control(const struct usb_ctrlrequest *ctrl, __u8 last_cmd, __u8 *scratch){
    switch(ctrl->bRequestType & USB_TYPE_MASK){
    case USB_TYPE_STANDARD: return plan_standard(ctrl, scratch);
    case USB_TYPE_CLASS: return plan_class(ctrl, last_cmd, scratch);
    default: return ep0_plan_act(EP0_STALL);
    }
}

/* Plan the reply, then do the ep0 I/O it names. */
static void handle_control(struct usb_ctrlrequest *ctrl){
    if(dbg){ char m[128]; snprintf(m,sizeof m,"  CTRL bRT=0x%02x bR=0x%02x wV=0x%04x wI=0x%04x wL=%u",
        ctrl->bRequestType, ctrl->bRequest, ctrl->wValue, ctrl->wIndex, ctrl->wLength); log_line(m); }
    __u8 scratch[260];
    struct ep0_plan p = plan_control(ctrl, last_feature_cmd, scratch);
    switch(p.action){
    case EP0_WRITE: ep0_write(p.data, p.len); return;
    case EP0_ACK: ep0_ack(); return;
    case EP0_SET_CONFIG: {
        __u32 power = 0x32; ioctl(fd, USB_RAW_IOCTL_VBUS_DRAW, power);
        ioctl(fd, USB_RAW_IOCTL_CONFIGURE);
        enable_endpoints();
        ep0_ack(); // OUT/no-data: complete via a zero-length read
        configured = 1; log_line("  SET_CONFIG: done");
        return;
    }
    case EP0_SET_REPORT: { // read the host's data, remember the command byte
        __u8 buf[64]; int r=ep0_read(buf,p.len);
        if(r>0) last_feature_cmd = buf[0]; // unnumbered report: data[0] is the command
        return; // ep0_read consumes the data stage + acks
    }
    case EP0_STALL: ep0_stall(); return;
    }
}

static void *stream_thread(void *arg){
    (void)arg; __u8 rep[64]; __u32 seq=0;
    while(running){
        if(configured && ctrl_ep>=0){
            memset(rep,0,sizeof rep);
            rep[0]=0x01; rep[1]=0x00; rep[2]=0x09; rep[3]=0x3c; memcpy(rep+4,&seq,4); seq++;
            char buf[sizeof(struct usb_raw_ep_io)+64]; struct usb_raw_ep_io *io=(void*)buf;
            io->ep=ctrl_ep; io->flags=0; io->length=64; memcpy(io->data,rep,64);
            ioctl(fd, USB_RAW_IOCTL_EP_WRITE, io); // blocks until the host polls the int IN ep
        }
        struct timespec ts={0, 8*1000*1000}; nanosleep(&ts,NULL);
    }
    return NULL;
}

int main(int argc, char **argv){
    int seconds = argc>1?atoi(argv[1]):120;
    for(int i=1;i<argc;i++){ if(!strcmp(argv[i],"nostream")) do_stream=0; }
    build_config();
    fd = open("/dev/raw-gadget", O_RDWR);
    if(fd<0){ perror("open /dev/raw-gadget"); return 1; }
    struct usb_raw_init init; memset(&init,0,sizeof init);
    strcpy((char*)init.driver_name, "dummy_udc");
    strcpy((char*)init.device_name, "dummy_udc.0");
    init.speed = USB_SPEED_HIGH;
    if(ioctl(fd, USB_RAW_IOCTL_INIT, &init)){ perror("INIT"); return 1; }
    if(ioctl(fd, USB_RAW_IOCTL_RUN)){ perror("RUN"); return 1; }
    log_line("raw_gadget Deck running (28DE:1205, controller on interface 2)");

    pthread_t th; if(do_stream) pthread_create(&th,NULL,stream_thread,NULL);

    struct timespec start; clock_gettime(CLOCK_MONOTONIC,&start);
    char ebuf[sizeof(struct usb_raw_event)+256];
    struct usb_raw_event *ev=(void*)ebuf;
    while(running){
        struct timespec n; clock_gettime(CLOCK_MONOTONIC,&n);
        if(n.tv_sec-start.tv_sec>=seconds) break;
        ev->type=0; ev->length=sizeof(struct usb_ctrlrequest);
        if(ioctl(fd, USB_RAW_IOCTL_EVENT_FETCH, ev)<0){ if(running) perror("EVENT_FETCH"); break; }
        if(ev->type==USB_RAW_EVENT_CONNECT){ log_line("CONNECT"); }
        else if(ev->type==USB_RAW_EVENT_CONTROL){ handle_control((struct usb_ctrlrequest*)ev->data); }
    }
    running=0; if(do_stream) pthread_join(th,NULL);
    log_line("exiting");
    return 0;
}
