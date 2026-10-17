PREFIX         ?= /usr/local
BINDIR         ?= $(PREFIX)/bin
MANDIR         ?= $(PREFIX)/share/man
DATADIR        ?= $(PREFIX)/share
SYSCONFDIR     ?= /etc
SYSTEMDUNITDIR ?= $(SYSCONFDIR)/systemd/system
BASHCOMPDIR    ?= $(DATADIR)/bash-completion/completions
ZSHCOMPDIR     ?= $(DATADIR)/zsh/site-functions
FISHCOMPDIR    ?= $(DATADIR)/fish/vendor_completions.d
LOGROTATEDIR   ?= $(SYSCONFDIR)/logrotate.d
LICENSEDIR     ?= $(DATADIR)/licenses/iowatchd
# Set to 0 to skip the init service files (e.g. for distro packages that handle them).
INSTALL_INIT   ?= systemd

CARGO ?= cargo
INSTALL ?= install
# Binary to install; override for prebuilt release tarballs (BIN=./iowatchd).
BIN ?= target/release/iowatchd

.PHONY: all build test install uninstall clean

all build:
	$(CARGO) build --release --locked

test:
	$(CARGO) test --locked

# DESTDIR is honoured for staged installs (packaging). Run `make` first:
# install does not build, so `sudo make install` never runs cargo as root.
install:
	@test -x $(BIN) || { echo "run 'make' first (or set BIN=)" >&2; exit 1; }
	$(INSTALL) -D -m 755 $(BIN) $(DESTDIR)$(BINDIR)/iowatchd
		$(INSTALL) -D -m 755 iowatchd-log $(DESTDIR)$(BINDIR)/iowatchd-log
	$(INSTALL) -D -m 755 iowatchd-notify $(DESTDIR)$(BINDIR)/iowatchd-notify
	$(INSTALL) -D -m 644 man/iowatchd.8 $(DESTDIR)$(MANDIR)/man8/iowatchd.8
	$(INSTALL) -D -m 644 man/iowatchd-log.1 $(DESTDIR)$(MANDIR)/man1/iowatchd-log.1
	$(INSTALL) -D -m 644 man/iowatchd.conf.5 $(DESTDIR)$(MANDIR)/man5/iowatchd.conf.5
	$(INSTALL) -D -m 644 completions/iowatchd.bash $(DESTDIR)$(BASHCOMPDIR)/iowatchd
	$(INSTALL) -D -m 644 completions/_iowatchd $(DESTDIR)$(ZSHCOMPDIR)/_iowatchd
	$(INSTALL) -D -m 644 completions/iowatchd.fish $(DESTDIR)$(FISHCOMPDIR)/iowatchd.fish
	$(INSTALL) -D -m 644 packaging/logrotate/iowatchd $(DESTDIR)$(LOGROTATEDIR)/iowatchd
	$(INSTALL) -D -m 644 LICENSE $(DESTDIR)$(LICENSEDIR)/LICENSE
	[ -f $(DESTDIR)$(SYSCONFDIR)/iowatchd.conf ] || \
	  $(INSTALL) -D -m 644 config.conf $(DESTDIR)$(SYSCONFDIR)/iowatchd.conf
ifeq ($(INSTALL_INIT),systemd)
	$(INSTALL) -d $(DESTDIR)$(SYSTEMDUNITDIR)
	sed 's|/usr/local/bin|$(BINDIR)|g' init/systemd/iowatchd.service \
	  > $(DESTDIR)$(SYSTEMDUNITDIR)/iowatchd.service
	chmod 644 $(DESTDIR)$(SYSTEMDUNITDIR)/iowatchd.service
endif

# The configuration file is left in place on purpose.
uninstall:
	rm -f $(DESTDIR)$(BINDIR)/iowatchd $(DESTDIR)$(BINDIR)/iowatchd-log $(DESTDIR)$(BINDIR)/iowatchd-notify
	rm -f $(DESTDIR)$(MANDIR)/man8/iowatchd.8 $(DESTDIR)$(MANDIR)/man1/iowatchd-log.1 \
	  $(DESTDIR)$(MANDIR)/man5/iowatchd.conf.5
	rm -f $(DESTDIR)$(BASHCOMPDIR)/iowatchd $(DESTDIR)$(ZSHCOMPDIR)/_iowatchd \
	  $(DESTDIR)$(FISHCOMPDIR)/iowatchd.fish $(DESTDIR)$(LOGROTATEDIR)/iowatchd
	rm -rf $(DESTDIR)$(LICENSEDIR)
	rm -f $(DESTDIR)$(SYSTEMDUNITDIR)/iowatchd.service

clean:
	$(CARGO) clean
