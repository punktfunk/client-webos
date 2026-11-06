#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <stdlib.h>
#include <fcntl.h>
#include <sys/ioctl.h>
#include <poll.h>
#include <pthread.h>
#include <time.h>
#define LXVO_DIR "/media/developer/apps/usr/palm/applications/io.unom.punktfunk.client-webos"
#include <wayland-client.h>
#include <wayland-webos-foreign-client-protocol.h>

typedef struct {
	const void *buf;
	uint32_t luma_offset, chroma_offset, length, stride, x, y, width, height, pixel_format;
} hal_vo_image;

typedef struct {
	uint32_t type, value, a2, a3, a4;
} dile_vo_config;

static int (*vo_open)(int, int);
static int (*vo_close)(int);
static int (*vo_config)(int, dile_vo_config);
static int (*hal_display)(int, hal_vo_image *);
static int vo_ch = -1;

static struct wl_event_queue *queue;
static struct wl_display *wrapper;
static struct wl_webos_foreign *foreign;
static struct wl_webos_exported *exported;
static struct wl_webos_imported *imported;
static struct wl_compositor *compositor;
static char window_id[128];

static void on_global_remove(void *data, struct wl_registry *reg, uint32_t name)
{
	(void)data;
	(void)reg;
	(void)name;
}

static void on_window_id(void *data, struct wl_webos_exported *exp, const char *id, uint32_t type)
{
	(void)data;
	(void)exp;
	(void)type;
	snprintf(window_id, sizeof(window_id), "%s", id);
}

static const struct wl_webos_exported_listener exported_listener = { on_window_id };

static struct wl_region *region(struct wl_compositor *c, int w, int h)
{
	struct wl_region *r = wl_compositor_create_region(c);
	wl_region_add(r, 0, 0, w, h);
	return r;
}

static void on_global(void *data, struct wl_registry *reg, uint32_t name, const char *iface, uint32_t ver)
{
	(void)data;
	(void)ver;
	if (!strcmp(iface, "wl_webos_foreign"))
		foreign = wl_registry_bind(reg, name, &wl_webos_foreign_interface, 1);
	else if (!strcmp(iface, "wl_compositor"))
		compositor = wl_registry_bind(reg, name, &wl_compositor_interface, 1);
}

static const struct wl_registry_listener registry_listener = { on_global, on_global_remove };

int pf_vo_punch(struct wl_display *display, struct wl_surface *surface, const char *context, int w, int h)
{
	if (imported)
		return 0;
	queue = wl_display_create_queue(display);
	wrapper = wl_proxy_create_wrapper(display);
	wl_proxy_set_queue((struct wl_proxy *)wrapper, queue);
	struct wl_registry *reg = wl_display_get_registry(wrapper);
	wl_registry_add_listener(reg, &registry_listener, NULL);
	wl_display_roundtrip_queue(display, queue);
	if (!foreign || !compositor)
		return -1;
	exported = wl_webos_foreign_export_element(foreign, surface, WL_WEBOS_FOREIGN_WEBOS_EXPORTED_TYPE_VIDEO_OBJECT);
	wl_webos_exported_add_listener(exported, &exported_listener, NULL);
	wl_webos_exported_set_exported_window(exported, region(compositor, w, h), region(compositor, w, h));
	for (int i = 0; i < 50 && !window_id[0]; i++)
		wl_display_roundtrip_queue(display, queue);
	if (!window_id[0])
		return -2;
	imported = wl_webos_foreign_import_element(foreign, window_id, WL_WEBOS_FOREIGN_WEBOS_EXPORTED_TYPE_VIDEO_OBJECT);
	wl_proxy_marshal((struct wl_proxy *)imported, 2, context);
	wl_display_roundtrip_queue(display, queue);
	return 0;
}

const char *pf_vo_window_id(void)
{
	return window_id;
}

typedef int (*lx_rw_t)(void *);
typedef int (*lx_fr_t)(void *, unsigned, unsigned);
static void *lx_h;
static int lx_fd = -1, lx_flags = 2, lx_lag = 1, lx_tmo = 25;
static volatile unsigned lx_disp, lx_drop, lx_nev;
static unsigned lx_k, lx_base;
static lx_rw_t lx_read_info;
static pthread_mutex_t lx_mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t lx_cv = PTHREAD_COND_INITIALIZER;
#define LX_RING 65536
static struct { double t; unsigned tag, w1, w2; } lx_ev[LX_RING];
static struct { double t0, t1, t2; unsigned k, disp; } lx_fl[LX_RING];
static unsigned lx_nfl, lx_dumped_ev, lx_dumped_fl;

static double lx_now(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}

static void *lx_thread(void *arg)
{
	(void)arg;
	for (;;) {
		struct pollfd p = { lx_fd, POLLIN | POLLPRI, 0 };
		int r = poll(&p, 1, 200);
		if (r <= 0 || !(p.revents & POLLIN))
			continue;
		memset((char *)lx_h + 8, 0, 56);
		unsigned tag = (unsigned)lx_read_info(lx_h);
		double t = lx_now();
		unsigned *w = (unsigned *)((char *)lx_h + 8);
		unsigned n = lx_nev++ % LX_RING;
		lx_ev[n].t = t;
		lx_ev[n].tag = tag;
		lx_ev[n].w1 = w[1];
		lx_ev[n].w2 = w[2];
		pthread_mutex_lock(&lx_mu);
		if (tag == 0x44495350u)
			lx_disp++;
		else if (tag == 0x44524f50u)
			lx_drop++;
		pthread_cond_broadcast(&lx_cv);
		pthread_mutex_unlock(&lx_mu);
	}
	return NULL;
}

static void lx_dump(void)
{
	FILE *f = fopen("/tmp/lxvo.log", "a");
	if (!f)
		return;
	for (; lx_dumped_ev < lx_nev && lx_dumped_ev < LX_RING; lx_dumped_ev++) {
		unsigned i = lx_dumped_ev;
		fprintf(f, "%.3f EV %08x w1=%x w2=%x\n", lx_ev[i].t, lx_ev[i].tag, lx_ev[i].w1, lx_ev[i].w2);
	}
	for (; lx_dumped_fl < lx_nfl && lx_dumped_fl < LX_RING; lx_dumped_fl++) {
		unsigned i = lx_dumped_fl;
		fprintf(f, "%.3f FR k=%u t1=%.3f t2=%.3f disp=%u\n", lx_fl[i].t0, lx_fl[i].k, lx_fl[i].t1, lx_fl[i].t2, lx_fl[i].disp);
	}
	fclose(f);
}

int pf_vo_open(int uhd)
{
	if (vo_ch >= 0)
		return 0;
	dlopen("libPmLogLib.so.3", RTLD_NOW | RTLD_GLOBAL);
	void *dile = dlopen("libdile_photo.so.1", RTLD_NOW);
	void *hal = dlopen("libhal_photo.so.2", RTLD_NOW);
	if (!dile || !hal)
		return -1;
	vo_open = dlsym(dile, "DILE_VO_Open");
	vo_close = dlsym(dile, "DILE_VO_Close");
	vo_config = dlsym(dile, "DILE_VO_Config");
	hal_display = dlsym(hal, "HAL_VO_DisplayPicture");
	if (!vo_open || !vo_close || !vo_config || !hal_display)
		return -2;
	int r = vo_open(0, uhd ? 2 : 1);
	if (r)
		return r;
	vo_ch = 0;
	dile_vo_config c = { 4, 13, 0, 0, 0 };
	vo_config(vo_ch, c);
	/* liblxv handle of channel 0 from the HAL channel table (libhal_photo md5 68bbd1f2) */
	uintptr_t hv = (uintptr_t)dlsym(hal, "HAL_VO_Open");
	void *lxv = dlopen("liblxv.so.2", RTLD_NOW);
	if (hv && lxv) {
		lx_h = *(void **)((hv & ~1u) - 0x2c1c + 0x160f0);
		lx_fr_t set_fr = (lx_fr_t)dlsym(lxv, "lxvo_set_framerate");
		if (lx_h && set_fr)
			set_fr(lx_h, 60, 1);
		FILE *lf = fopen(LXVO_DIR "/lxvo-dg", "r");
		if (lf) {
			if (fscanf(lf, "%d %d %d", &lx_flags, &lx_lag, &lx_tmo) < 1)
				lx_flags = 0;
			fclose(lf);
		}
		if (lx_h) {
			lx_read_info = (lx_rw_t)dlsym(lxv, "lxvo_read_info");
			lx_fd = *(int *)lx_h;
			if (lx_flags && lx_read_info && lx_fd >= 0) {
				pthread_t th;
				pthread_create(&th, NULL, lx_thread, NULL);
			} else {
				lx_flags = 0;
			}
		} else {
			lx_flags = 0;
		}
	}
	return 0;
}

int pf_vo_show(const uint8_t *luma, const uint8_t *chroma, uint32_t stride, uint32_t w, uint32_t h)
{
	if (vo_ch < 0)
		return -1;
	double t0 = lx_flags ? lx_now() : 0;
	if (lx_k == 0)
		lx_base = lx_disp + lx_drop;
	if ((lx_flags & 2) && lx_k >= 1) {
		struct timespec ts;
		clock_gettime(CLOCK_REALTIME, &ts);
		long ns = ts.tv_nsec + (long)lx_tmo * 1000000L;
		ts.tv_sec += ns / 1000000000L;
		ts.tv_nsec = ns % 1000000000L;
		pthread_mutex_lock(&lx_mu);
		while ((int)(lx_disp + lx_drop - lx_base) < (int)lx_k - lx_lag)
			if (pthread_cond_timedwait(&lx_cv, &lx_mu, &ts))
				break;
		pthread_mutex_unlock(&lx_mu);
	}
	double t1 = lx_flags ? lx_now() : 0;
	hal_vo_image img = { luma, 0, (uint32_t)(chroma - luma), stride * h * 3 / 2, stride, 0, 0, w, h, 13 };
	int ret = hal_display(vo_ch, &img);
	if (lx_flags) {
		unsigned n = lx_nfl++ % LX_RING;
		lx_fl[n].t0 = t0;
		lx_fl[n].t1 = t1;
		lx_fl[n].t2 = lx_now();
		lx_fl[n].k = lx_k;
		lx_fl[n].disp = lx_disp;
		if (!(lx_k % 120) && lx_k)
			lx_dump();
	}
	lx_k++;
	return ret;
}

void pf_vo_close(void)
{
	if (lx_flags)
		lx_dump();
	if (vo_ch >= 0) {
		vo_close(vo_ch);
		vo_ch = -1;
	}
	if (imported) {
		wl_webos_imported_detach_punchthrough(imported);
		wl_webos_imported_destroy(imported);
		imported = NULL;
	}
	if (exported) {
		wl_webos_exported_destroy(exported);
		exported = NULL;
	}
	window_id[0] = 0;
}
