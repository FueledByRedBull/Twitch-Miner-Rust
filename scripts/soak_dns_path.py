"""One bounded read-only DNS path probe; no credentials, configuration or watcher.

Two public resolvers should answer and the documentation-only address should
time out; an answer from it means DNS is being intercepted on the path.
"""
import datetime as dt
import json
import secrets
import socket
import struct
import time


def main():
    results = []
    for address, label in [('1.1.1.1', 'cloudflare'), ('8.8.8.8', 'google'),
                           ('192.0.2.1', 'documentation-address')]:
        query_id = secrets.randbits(16)
        question = b'\x07example\x03com\x00' + struct.pack('!HH', 1, 1)
        query = struct.pack('!HHHHHH', query_id, 0x0100, 1, 0, 0, 0) + question
        start = time.monotonic()
        row = {'target': label}
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
                sock.settimeout(2)
                sock.sendto(query, (address, 53))
                data, _ = sock.recvfrom(4096)
            header = struct.unpack('!HHHHHH', data[:12])
            valid = (header[0] == query_id and header[1] & 0x8000
                     and header[1] & 15 == 0 and header[3] > 0
                     and data[12:12 + len(question)] == question)
            row['outcome'] = 'answer' if valid else 'invalid-response'
        except socket.timeout:
            row['outcome'] = 'timeout'
        except (OSError, struct.error) as error:
            row['outcome'] = 'probe-error'
            row['error_type'] = type(error).__name__
        row['elapsed_ms'] = round((time.monotonic() - start) * 1000, 1)
        results.append(row)
    print(json.dumps({'utc': dt.datetime.now(dt.timezone.utc).isoformat(),
                      'results': results,
                      'expected_behavior': [r['outcome'] for r in results] == ['answer', 'answer', 'timeout'],
                      'limitation': 'These sampled destinations do not establish historical uptime or rule out all interception.'}))


if __name__ == '__main__':
    main()
