// Viewer-response CloudFront Function for the backup website (website.tf).
//
// The metadata collections (*.json.gz) are gzipped JSON. Mark them as
// downloads, keeping their file name, so that browsers save them rather than
// try to show them.
function handler(event) {
    var response = event.response;
    var uri = event.request.uri;
    var ok = response.statusCode === 200 || response.statusCode === 206;

    if (ok && /\.json\.gz$/.test(uri)) {
        var name = uri.substring(uri.lastIndexOf('/') + 1);
        // Collection names are plain ASCII; anything else gets no name
        // rather than a header that would need quoting or encoding.
        var disposition = /^[A-Za-z0-9._-]+$/.test(name) ? 'attachment; filename="' + name + '"' : 'attachment';
        response.headers['content-disposition'] = { value: disposition };
    }
    return response;
}
