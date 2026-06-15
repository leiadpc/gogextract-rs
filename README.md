# GOGExtract-RS
gogextract.py rewrite in Rust with enhancements

- Progress bar
- File decompression
- Multithread performance
- Optinal GUI or CLI only builds (use `cargo build --no-default-features` for CLI only build)
- Can use innoextract for Windows installers if installed 
<br>
A few notable differences in operation

- Only extracts\decompresses game data not the the install script or mojosetup files
- Only works with installers that use standard zip compression for the game files
- Extracts into a temp folder first then moves into a folder named after the install file by default






# Orginal GOG Extract Script

Script for unpacking GOG Linux installers.

Explanation of how it works is in my [blog post].

## Usage

`gogextract.py <input file> <output dir>`

Output files will be named `unpacker.sh`, `mojosetup.tar.gz` and `data.zip`.

## License

[MIT](LICENSE)

[blog post]: https://yepoleb.github.io/blog/2016/10/09/how-the-gog-linux-installers-work/
