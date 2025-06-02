import os
import shutil
import toml
import argparse
import logging
import binascii
import struct
import base64
import json
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
from cryptography.hazmat.backends import default_backend
import concurrent.futures
from concurrent.futures import ProcessPoolExecutor

logging.basicConfig(
    level=logging.INFO,
    format='%(asctime)s %(levelname)s: %(message)s',
    datefmt='%Y-%m-%d %H:%M:%S'
)

def ncmdump(filepath, target_folder):
    log = logging.getLogger("ncmdump")
    try:
        filename = os.path.basename(filepath)
        if not filename.endswith('.ncm'): return  # noqa: E701
        filename = filename[:-4]
        for ftype in ['mp3', 'flac']:
            fname = os.path.join(target_folder, f'{filename}.{ftype}')
            if os.path.isfile(fname):
                log.warning(f'Skipping "{filepath}" due to existing file "{fname}"')
                return
        log.info(f'Converting "{filepath}"')
        core_key = binascii.a2b_hex('687A4852416D736F356B496E62617857')
        meta_key = binascii.a2b_hex('2331346C6A6B5F215C5D2630553C2728')
        
        def unpad(s):
            return s[0:-(s[-1] if isinstance(s[-1], int) else ord(s[-1]))]

        with open(filepath, 'rb') as f:
            header = f.read(8)
            assert binascii.b2a_hex(header) == b'4354454e4644414d'
            f.seek(2, 1)
            key_length = f.read(4)
            key_length = struct.unpack('<I', bytes(key_length))[0]
            key_data = f.read(key_length)
            key_data_array = bytearray(key_data)
            for i in range(0, len(key_data_array)):
                key_data_array[i] ^= 0x64
            key_data = bytes(key_data_array)
            cryptor = Cipher(algorithms.AES(core_key), modes.ECB(), backend=default_backend()).decryptor()
            key_data = unpad(cryptor.update(key_data) + cryptor.finalize())[17:]
            key_length = len(key_data)
            key_data = bytearray(key_data)
            key_box = bytearray(range(256))
            c = 0
            last_byte = 0
            key_offset = 0
            for i in range(256):
                swap = key_box[i]
                c = (swap + last_byte + key_data[key_offset]) & 0xff
                key_offset += 1
                if key_offset >= key_length:
                    key_offset = 0
                key_box[i] = key_box[c]
                key_box[c] = swap
                last_byte = c
            meta_length = f.read(4)
            meta_length = struct.unpack('<I', bytes(meta_length))[0]
            meta_data = f.read(meta_length)
            meta_data_array = bytearray(meta_data)
            for i in range(0, len(meta_data_array)):
                meta_data_array[i] ^= 0x63
            meta_data = bytes(meta_data_array)
            meta_data = base64.b64decode(meta_data[22:])
            cryptor = Cipher(algorithms.AES(meta_key), modes.ECB(), backend=default_backend()).decryptor()
            meta_data = unpad(cryptor.update(meta_data) + cryptor.finalize()).decode('utf-8')[6:]
            meta_data = json.loads(meta_data)
            crc32 = f.read(4)
            crc32 = struct.unpack('<I', bytes(crc32))[0]
            f.seek(5, 1)
            image_size = f.read(4)
            image_size = struct.unpack('<I', bytes(image_size))[0]
            target_filename = os.path.join(target_folder, f'{filename}.{meta_data["format"]}')
            with open(target_filename, 'wb') as m:
                chunk = bytearray()
                while True:
                    chunk = bytearray(f.read(0x8000))
                    chunk_length = len(chunk)
                    if not chunk:
                        break
                    for i in range(1, chunk_length + 1):
                        j = i & 0xff
                        chunk[i - 1] ^= key_box[(key_box[j] + key_box[(key_box[j] + j) & 0xff]) & 0xff]
                    m.write(chunk)
        log.info(f'Converted file saved at "{target_filename}"')
        return target_filename
    except KeyboardInterrupt:
        log.warning('Aborted')
        quit()

def get_music_dict(folder):
    """
    返回 {文件名（无后缀）: {'size': 文件大小, 'path': 文件路径}} 字典
    """
    music_dict = {}
    for root, _, files in os.walk(folder):
        for f in files:
            name, ext = os.path.splitext(f)
            if ext.lower() in ['.mp3', '.flac', '.ncm']:
                path = os.path.join(root, f)
                size = os.path.getsize(path)
                music_dict[name] = {'size': size, 'path': path}
    return music_dict

def process_song_mp(args):
    name, wf_dict, sf = args
    try:
        src_info = wf_dict[name]
        src_path = src_info['path']
        _, ext = os.path.splitext(src_path)
        if ext == ".ncm":
            ncmdump(src_path, sf)
        else:
            dst_path = os.path.join(sf, os.path.basename(src_path))
            shutil.copy2(src_path, dst_path)
        return (name, None)
    except Exception as e:
        return (name, str(e))

def main():
    parser = argparse.ArgumentParser(description="网易云音乐转换器")
    parser.add_argument("-c", "--config", default="config.toml", help="配置文件路径")
    args = parser.parse_args()
    
    # 1. 读取配置文件
    config_path = args.config
    if not os.path.exists(config_path):
        logging.error(f"缺少配置文件: {config_path}")
        logging.info("请创建 config.toml, 内容示例:")
        logging.info("""source = "/path/to/网易云"
destination = "/path/to/你的歌库""")
        return
    config = toml.load(config_path)
    wf = config.get("source")
    sf = config.get("destination")
    if not (wf and sf):
        logging.error("配置文件缺少 source 或 destination 字段")
        return
    logging.info(f"源文件夹: {wf}\n目标文件夹: {sf}")

    if not os.path.exists(wf):
        logging.error(f"源文件夹不存在: {wf}")
        return
    if not os.path.exists(sf):
        logging.error(f"目标文件夹不存在,自动创建")
        os.makedirs(sf)
        
    # 2. 维护歌库
    wf_dict = get_music_dict(wf)
    sf_dict = get_music_dict(sf)
    # 3. 比对，消除同名且大小相近的歌
    wf_keys = set(wf_dict.keys())
    sf_keys = set(sf_dict.keys())
    to_add = []
    for name in list(wf_keys):
        if name in sf_dict:
            size1 = wf_dict[name]['size']
            size2 = sf_dict[name]['size']
            if abs(size1 - size2) / max(size1, size2) < 0.03:
                # 认为是同一首
                wf_keys.discard(name)
                sf_keys.discard(name)
    # 剩下 wf_keys 里的是新增
    to_add = [name for name in wf_keys]
    logging.info(f"需要新增的歌曲: {to_add}")
    # 4. 处理新增歌曲（多进程）
    with ProcessPoolExecutor(max_workers=8) as executor:
        futures = [executor.submit(process_song_mp, (name, wf_dict, sf)) for name in to_add]
        for future in concurrent.futures.as_completed(futures):
            name, error = future.result()
            if error:
                logging.error(f"处理歌曲 {name} 时出错: {error}")
            else:
                logging.info(f"已处理: {name}")


if __name__ == "__main__":
    main()
