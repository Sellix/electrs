#!/usr/bin/env python3
import hashlib
import sys
import time
import argparse

import client

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--testnet', action='store_true')
    parser.add_argument('address', nargs='+')
    args = parser.parse_args()

    if args.testnet:
        port = 60001
        from pycoin.symbols.xtn import network
    else:
        port = 50001
        from pycoin.symbols.btc import network

    conn = client.Client(('localhost', port))
    for addr in args.address:
        script = network.parse.address(addr).script()
        script_hash = hashlib.sha256(script).digest()[::-1].hex()
        t = time.time()
        reply = conn.call('blockchain.scripthash.subscribe', script_hash)
        dt = time.time() - t
        print('{} subscription took {:.3f} ms'.format(addr, dt * 1e3))

        t = time.time()
        reply = conn.call('blockchain.scripthash.get_history', script_hash)
        txs_count = len(reply['result'])
        dt = time.time() - t
        print('{} has {} txs, took {:.3f} ms'.format(addr, txs_count, dt * 1e3))

    input("Press ENTER to exit...")


if __name__ == '__main__':
    main()
