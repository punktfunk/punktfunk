---
title: Browser Client
description: Stream from a Punktfunk host in a browser tab — turn on browser streaming, run the page on your network, pair once, and play.
---

Stream in a browser tab with nothing to install: you run the page once on your network, and any
browser there opens it. The browser client is a preview; the [native apps](/docs/clients) give the
better picture and the lower latency.

It needs a browser with WebTransport and WebCodecs: current Chrome, Edge, Safari or Firefox. On a
Samsung TV the same page installs as an app: [Samsung TV](/docs/samsung-tv).

## 1. Turn on browser streaming on the host

1. In the host's [web console](/docs/web-console), open **Host → Settings** and turn on **Browser
   streaming**.
2. Click **Show advanced** and set **Browser origins** to the address you will open the page at,
   for example `https://192.168.1.10:8443`. Left empty, any page open in a browser on your network
   can reach the host.
3. Click **Restart Punktfunk**.
4. Open UDP 9778 on the host: it is in the `punktfunk-native` firewall profile
   ([Ports](/docs/ports)). The video goes from the host to the browser directly.

## 2. Run the page

The page runs in a container on any Linux machine on your network; the host itself will do. With
Docker installed:

```sh
curl -O https://raw.githubusercontent.com/punktfunk/client-web/main/deploy/compose.yaml
```

Set `TLS_NAMES` in `compose.yaml` to that machine's address, then start it:

```sh
docker compose up -d
```

The container finds hosts that announce themselves on your network. Settings, under
`environment:` in `compose.yaml`:

| Setting | Default | |
|---|---|---|
| `TLS_NAMES` | — | The names and addresses you open the page at, for its certificate |
| `PUNKTFUNK_HOSTS` | — | Hosts it can't find: `name=address[:port]`, comma-separated |
| `TLS` | `self-signed` | `off` behind a reverse proxy, or `cert.pem,key.pem` |
| `LISTEN` | `0.0.0.0:8443` | Address and port the page is served on |
| `DISCOVER` | on | `0` stops it looking for hosts |
| `ADD_HOSTS` | on | `0` stops it reaching a host typed by address. Set it when the page is reachable from outside your network |

Behind a reverse proxy that has a certificate, or on a tailnet with Tailscale's certificate, use
`compose.proxy.yaml` or `compose.tailscale.yaml` from the same
[`deploy/` folder](https://github.com/punktfunk/client-web/tree/main/deploy). The tailnet one also
lets you play away from home.

## 3. Open it and pair

1. Open `https://<that machine>:8443` and accept the certificate warning once.
2. Pick your host. For one the page did not find, click **Add a host** and type its address.
3. Choose **Request access**, then approve the browser in the console under **Devices → Waiting
   for approval** ([Pairing](/docs/pairing#approve-it-from-the-console-no-pin)). Or click **Pair a
   device** in the console and type its PIN into the page.

The browser stays paired. Clearing the site's data unpairs it, and a private window is a new device
every time.

## 4. Stream

- Pick a title from the library, or **Stream the desktop**. **Resume** continues the title the host
  is running.
- **Menu** in the bar at the top, Ctrl+Alt+Shift+O, or Back+A on a controller opens the quick menu.
  **End stream** closes the title; **Disconnect, keep the game running** leaves it up. The other
  keys are every client's ([Input](/docs/input#getting-your-input-back)).
- The gear opens **Settings**: stream size, frame rate, the mouse, the statistics overlay. The
  bitrate follows the connection unless you turn **Automatic bitrate** off.

The page uses what the browser offers: HEVC and AV1 where it decodes them, HDR on an HDR display
where it has WebGPU (unless **Video plane** is **WebGL2**), controller rumble where it drives the
pad's motors, and the microphone from the quick menu where it can encode Opus.

## Console mode

The controller button in the top bar switches the page to Punktfunk Console, the interface every
client draws for a TV and a gamepad: hosts, pairing, the library and settings, all by D-pad.
**Leave console mode** switches back. To start in it, open `https://<that machine>:8443/?ui=console`.

## Links, wake and power

- **Copy a link** on a host card copies a link that opens the page on that host. A link always
  asks before it connects.
- **Wake** on a sleeping host's card sends a Wake-on-LAN packet from the page's machine, which has
  to share the host's network ([Wake-on-LAN](/docs/wake-on-lan)).
- **Host**, the server button in the library's bar, runs the host's power actions this browser is
  allowed ([Host power](/docs/host-power)) and sends the page's log to the host.

## Troubleshooting

### The page asks you to accept the host's certificate

The page reached the host directly, and the browser does not trust the host's own certificate yet.
Open the link it shows, accept the warning, and choose **I have accepted it**. If it keeps asking,
**Browser streaming** is off on that host (step 1).

### The stream never starts, or drops as it starts

UDP 9778 is blocked between the browser and the host. Open it on the host (step 1) and on any
firewall between them.

### Safari on a Mac never connects

**Auto proxy discovery** is on for the Mac's network connection, and Safari hands the stream to a
proxy. Turn it off in **System Settings → Network → your connection → Details → Proxies**.
