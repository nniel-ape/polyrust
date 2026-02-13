FROM alpine:3.21
RUN apk add --no-cache openssh-client autossh redsocks iptables
COPY redsocks.conf /etc/redsocks.conf
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]
