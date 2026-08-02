/* hds_query — ethtool-netlink RINGS_GET/SET probe for tcp-data-split
 * (HDS) on kernels whose installed ethtool binary predates the ring
 * attr (the field box ships ethtool 5.13). Recreates the 2026-08-02
 * interface-frontier campaign's read-only probe (the original was lost
 * in a re-provision) and adds the SET arm for the Phase-0 verdict.
 *
 * Build: gcc -O2 -o hds_query hds_query.c    (gcc 8.5-clean, no libnl)
 * Usage: hds_query <ifname>                  # read-only GET
 *        hds_query <ifname> set on|off       # RINGS_SET tcp-data-split
 *
 * Attr ordinals from include/uapi/linux/ethtool_netlink_generated.h
 * (6.19.14): RINGS_GET=15 RINGS_SET=16, HEADER=1 (nest: DEV_NAME=2),
 * TCP_DATA_SPLIT=11 (u8: 0 unknown / 1 disabled / 2 enabled),
 * HDS_THRESH=17, HDS_THRESH_MAX=18.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <sys/socket.h>
#include <linux/netlink.h>
#include <linux/genetlink.h>

#define ETHTOOL_GENL_NAME "ethtool"
#define ETHTOOL_MSG_RINGS_GET 15
#define ETHTOOL_MSG_RINGS_SET 16
#define ETHTOOL_A_RINGS_HEADER 1
#define ETHTOOL_A_RINGS_TCP_DATA_SPLIT 11
#define ETHTOOL_A_RINGS_HDS_THRESH 17
#define ETHTOOL_A_RINGS_HDS_THRESH_MAX 18
#define ETHTOOL_A_HEADER_DEV_NAME 2

static int nl_fd = -1;
static unsigned int nl_seq = 1;

struct nlbuf {
	char b[8192];
	int len;
};

static void put_hdr(struct nlbuf *m, unsigned short type, unsigned short flags)
{
	struct nlmsghdr *nh = (struct nlmsghdr *)m->b;
	memset(m->b, 0, sizeof(m->b));
	nh->nlmsg_type = type;
	nh->nlmsg_flags = flags;
	nh->nlmsg_seq = nl_seq++;
	m->len = NLMSG_HDRLEN;
}

static void put_genl(struct nlbuf *m, unsigned char cmd)
{
	struct genlmsghdr *gh = (struct genlmsghdr *)(m->b + m->len);
	gh->cmd = cmd;
	gh->version = 1;
	m->len += GENL_HDRLEN;
}

static struct nlattr *put_attr(struct nlbuf *m, unsigned short type,
			       const void *data, int len)
{
	struct nlattr *a = (struct nlattr *)(m->b + m->len);
	a->nla_type = type;
	a->nla_len = NLA_HDRLEN + len;
	if (data)
		memcpy((char *)a + NLA_HDRLEN, data, len);
	m->len += NLA_ALIGN(a->nla_len);
	return a;
}

static int xchg(struct nlbuf *m, struct nlbuf *r)
{
	struct sockaddr_nl sa;
	struct nlmsghdr *nh = (struct nlmsghdr *)m->b;
	ssize_t n;

	nh->nlmsg_len = m->len;
	memset(&sa, 0, sizeof(sa));
	sa.nl_family = AF_NETLINK;
	if (sendto(nl_fd, m->b, m->len, 0, (struct sockaddr *)&sa,
		   sizeof(sa)) < 0)
		return -errno;
	n = recv(nl_fd, r->b, sizeof(r->b), 0);
	if (n < 0)
		return -errno;
	r->len = (int)n;
	return 0;
}

static int resolve_family(void)
{
	struct nlbuf m, r;
	struct nlmsghdr *nh;

	put_hdr(&m, GENL_ID_CTRL, NLM_F_REQUEST);
	put_genl(&m, CTRL_CMD_GETFAMILY);
	put_attr(&m, CTRL_ATTR_FAMILY_NAME, ETHTOOL_GENL_NAME,
		 (int)strlen(ETHTOOL_GENL_NAME) + 1);
	if (xchg(&m, &r) < 0)
		return -1;
	for (nh = (struct nlmsghdr *)r.b; NLMSG_OK(nh, (unsigned int)r.len);
	     nh = NLMSG_NEXT(nh, r.len)) {
		struct nlattr *a;
		int rem;

		if (nh->nlmsg_type == NLMSG_ERROR)
			return -1;
		a = (struct nlattr *)((char *)nh + NLMSG_HDRLEN + GENL_HDRLEN);
		rem = nh->nlmsg_len - NLMSG_HDRLEN - GENL_HDRLEN;
		while (rem >= NLA_HDRLEN) {
			if ((a->nla_type & NLA_TYPE_MASK) ==
			    CTRL_ATTR_FAMILY_ID)
				return *(unsigned short *)((char *)a +
							   NLA_HDRLEN);
			rem -= NLA_ALIGN(a->nla_len);
			a = (struct nlattr *)((char *)a +
					      NLA_ALIGN(a->nla_len));
		}
	}
	return -1;
}

static const char *split_str(unsigned char v)
{
	switch (v) {
	case 1: return "off (disabled)";
	case 2: return "on (enabled)";
	default: return "unknown";
	}
}

int main(int argc, char **argv)
{
	const char *ifname;
	int fam, do_set = 0;
	unsigned char set_val = 0;
	struct nlbuf m, r;
	struct nlattr *hdr;
	struct nlmsghdr *nh;

	if (argc < 2) {
		fprintf(stderr,
			"usage: %s <ifname> [set on|off]\n", argv[0]);
		return 2;
	}
	ifname = argv[1];
	if (argc >= 4 && !strcmp(argv[2], "set")) {
		do_set = 1;
		set_val = strcmp(argv[3], "on") ? 1 : 2;
	}

	nl_fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_GENERIC);
	if (nl_fd < 0) {
		perror("socket");
		return 1;
	}
	fam = resolve_family();
	if (fam < 0) {
		fprintf(stderr, "cannot resolve ethtool genl family\n");
		return 1;
	}

	put_hdr(&m, (unsigned short)fam,
		do_set ? (NLM_F_REQUEST | NLM_F_ACK) : NLM_F_REQUEST);
	put_genl(&m, do_set ? ETHTOOL_MSG_RINGS_SET : ETHTOOL_MSG_RINGS_GET);
	hdr = put_attr(&m, ETHTOOL_A_RINGS_HEADER | NLA_F_NESTED, NULL, 0);
	put_attr(&m, ETHTOOL_A_HEADER_DEV_NAME, ifname,
		 (int)strlen(ifname) + 1);
	hdr->nla_len = (unsigned short)((m.b + m.len) - (char *)hdr);
	if (do_set)
		put_attr(&m, ETHTOOL_A_RINGS_TCP_DATA_SPLIT, &set_val, 1);

	if (xchg(&m, &r) < 0) {
		perror("netlink");
		return 1;
	}

	for (nh = (struct nlmsghdr *)r.b; NLMSG_OK(nh, (unsigned int)r.len);
	     nh = NLMSG_NEXT(nh, r.len)) {
		if (nh->nlmsg_type == NLMSG_ERROR) {
			struct nlmsgerr *e = (struct nlmsgerr *)NLMSG_DATA(nh);

			if (e->error) {
				fprintf(stderr, "%s: kernel says: %s\n",
					do_set ? "RINGS_SET" : "RINGS_GET",
					strerror(-e->error));
				return 1;
			}
			printf("%s %s: ACK\n",
			       do_set ? "RINGS_SET" : "RINGS_GET", ifname);
			return 0;
		}
		if (nh->nlmsg_type == (unsigned short)fam) {
			struct nlattr *a = (struct nlattr *)((char *)nh +
					NLMSG_HDRLEN + GENL_HDRLEN);
			int rem = nh->nlmsg_len - NLMSG_HDRLEN - GENL_HDRLEN;
			int seen = 0;

			while (rem >= NLA_HDRLEN) {
				unsigned short t = a->nla_type & NLA_TYPE_MASK;
				unsigned char *p =
					(unsigned char *)a + NLA_HDRLEN;

				if (t == ETHTOOL_A_RINGS_TCP_DATA_SPLIT) {
					printf("%s tcp-data-split: %s\n",
					       ifname, split_str(*p));
					seen = 1;
				} else if (t == ETHTOOL_A_RINGS_HDS_THRESH) {
					printf("%s hds-thresh: %u\n", ifname,
					       *(unsigned int *)p);
				} else if (t ==
					   ETHTOOL_A_RINGS_HDS_THRESH_MAX) {
					printf("%s hds-thresh-max: %u\n",
					       ifname, *(unsigned int *)p);
				}
				rem -= NLA_ALIGN(a->nla_len);
				a = (struct nlattr *)((char *)a +
						      NLA_ALIGN(a->nla_len));
			}
			if (!seen)
				printf("%s tcp-data-split: ATTR NOT REPORTED\n",
				       ifname);
			return 0;
		}
	}
	fprintf(stderr, "no reply parsed\n");
	return 1;
}
