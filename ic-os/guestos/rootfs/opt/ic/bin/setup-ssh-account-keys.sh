#!/bin/bash

set -e

# Set up ssh keys for the role accounts: This is required to allow
# key-based login to these accounts

# TBD: should only allow root ssh key for test builds

# Bind mount to enable root access to .ssh in RO environment
mount --bind /run/ic-node/root/.ssh /root/.ssh

for ACCOUNT in root backup readonly admin; do
    HOMEDIR=$(getent passwd "${ACCOUNT}" | cut -d: -f6)
    GROUP=$(id -ng "${ACCOUNT}")

    mkdir -p "${HOMEDIR}/.ssh"

    if [ "${ACCOUNT}" != "root" ]; then
        chmod 700 "${HOMEDIR}" "${HOMEDIR}/.ssh"
        chown -R "${ACCOUNT}:${GROUP}" "${HOMEDIR}"
        restorecon -r "${HOMEDIR}"
    fi

    AUTHORIZED_SSH_KEYS="/boot/config/accounts_ssh_authorized_keys/${ACCOUNT}"
    if [ -e "${AUTHORIZED_SSH_KEYS}" ]; then
        cp -L "${AUTHORIZED_SSH_KEYS}" "${HOMEDIR}/.ssh/authorized_keys"
        chown "${ACCOUNT}:${GROUP}" "${HOMEDIR}/.ssh/authorized_keys"
        chmod 600 "${HOMEDIR}/.ssh/authorized_keys"
        restorecon "${HOMEDIR}/.ssh/authorized_keys"
    fi
done
