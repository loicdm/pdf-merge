mod image;
mod pagesize;

use lopdf::{Bookmark, Document, Object, ObjectId};
use printpdf::{ImageTransform, Mm, PdfDocument};
use std::{cmp::max, path::PathBuf, process::abort};

use image::{
    alpha_remover::RemoveAlpha, image_reader::read_image_from_file,
    image_transform::get_image_transform_for_page_size, image_x_object::get_image_dimension_in_mm,
};
use pagesize::PageSizeInMm;

const MIN_WIDTH_IN_MM: f64 = 210.0;
const MIN_HEIGHT_IN_MM: f64 = 297.0;

use std::{collections::BTreeMap, env, path::Path, process::exit};

use glob::glob;
//use lopdf::{Bookmark, Document, Object, ObjectId};

use std::process;

// imports the `image` library with the exact version that we are using
//use printpdf::*;

use rfd::FileDialog;
use std::convert::From;

use colored::Colorize;

//use image_crate::codecs::{bmp::BmpDecoder, jpeg::JpegDecoder, png::PngDecoder};

fn usage() {
    println!("Usage: pdf-merge <input_directory> <output_file>");
    println!("\nArguments:");
    println!("\t<input_directory>  Directory where the tool will search for .pdf files");
    println!("\t<output_file>  File to save the merged pdf result");
}

fn merge_documents(input_documents: Vec<Document>) -> Document {
    // Define a starting `max_id` (will be used as start index for object_ids).
    let mut max_id = 1;
    let mut pagenum = 1;
    // Collect all Documents Objects grouped by a map
    let mut documents_pages = BTreeMap::new();
    let mut documents_objects = BTreeMap::new();
    let mut document = Document::with_version("1.5");

    for mut doc in input_documents {
        let mut first = false;
        doc.renumber_objects_with(max_id);

        max_id = doc.max_id + 1;

        documents_pages.extend(
            doc.get_pages()
                .into_iter()
                .map(|(_, object_id)| {
                    if !first {
                        let bookmark = Bookmark::new(
                            String::from(format!("Page_{}", pagenum)),
                            [0.0, 0.0, 1.0],
                            0,
                            object_id,
                        );
                        document.add_bookmark(bookmark, None);
                        first = true;
                        pagenum += 1;
                    }

                    (object_id, doc.get_object(object_id).unwrap().to_owned())
                })
                .collect::<BTreeMap<ObjectId, Object>>(),
        );
        documents_objects.extend(doc.objects);
    }

    // "Catalog" and "Pages" are mandatory.
    let mut catalog_object: Option<(ObjectId, Object)> = None;
    let mut pages_object: Option<(ObjectId, Object)> = None;

    // Process all objects except "Page" type
    for (object_id, object) in documents_objects.iter() {
        // We have to ignore "Page" (as are processed later), "Outlines" and "Outline" objects.
        // All other objects should be collected and inserted into the main Document.
        match object.type_name().unwrap_or(b"") {
            b"Catalog" => {
                // Collect a first "Catalog" object and use it for the future "Pages".
                catalog_object = Some((
                    if let Some((id, _)) = catalog_object {
                        id
                    } else {
                        *object_id
                    },
                    object.clone(),
                ));
            }
            b"Pages" => {
                // Collect and update a first "Pages" object and use it for the future "Catalog"
                // We have also to merge all dictionaries of the old and the new "Pages" object
                if let Ok(dictionary) = object.as_dict() {
                    let mut dictionary = dictionary.clone();
                    if let Some((_, ref object)) = pages_object {
                        if let Ok(old_dictionary) = object.as_dict() {
                            dictionary.extend(old_dictionary);
                        }
                    }

                    pages_object = Some((
                        if let Some((id, _)) = pages_object {
                            id
                        } else {
                            *object_id
                        },
                        Object::Dictionary(dictionary),
                    ));
                }
            }
            b"Page" => {}     // Ignored, processed later and separately
            b"Outlines" => {} // Ignored, not supported yet
            b"Outline" => {}  // Ignored, not supported yet
            _ => {
                document.objects.insert(*object_id, object.clone());
            }
        }
    }

    // If no "Pages" object found, abort.
    if pages_object.is_none() {
        println!("Pages root not found.");

        return document;
    }

    // Iterate over all "Page" objects and collect into the parent "Pages" created before
    for (object_id, object) in documents_pages.iter() {
        if let Ok(dictionary) = object.as_dict() {
            let mut dictionary = dictionary.clone();
            dictionary.set("Parent", pages_object.as_ref().unwrap().0);

            document
                .objects
                .insert(*object_id, Object::Dictionary(dictionary));
        }
    }

    // If no "Catalog" found, abort.
    if catalog_object.is_none() {
        println!("Catalog root not found.");

        return document;
    }

    let catalog_object = catalog_object.unwrap();
    let pages_object = pages_object.unwrap();

    // Build a new "Pages" with updated fields
    if let Ok(dictionary) = pages_object.1.as_dict() {
        let mut dictionary = dictionary.clone();

        // Set new pages count
        dictionary.set("Count", documents_pages.len() as u32);

        // Set new "Kids" list (collected from documents pages) for "Pages"
        dictionary.set(
            "Kids",
            documents_pages
                .into_iter()
                .map(|(object_id, _)| Object::Reference(object_id))
                .collect::<Vec<_>>(),
        );

        document
            .objects
            .insert(pages_object.0, Object::Dictionary(dictionary));
    }

    // Build a new "Catalog" with updated fields
    if let Ok(dictionary) = catalog_object.1.as_dict() {
        let mut dictionary = dictionary.clone();
        dictionary.set("Pages", pages_object.0);
        dictionary.remove(b"Outlines"); // Outlines not supported in merged PDFs

        document
            .objects
            .insert(catalog_object.0, Object::Dictionary(dictionary));
    }

    document.trailer.set("Root", catalog_object.0);

    // Update the max internal ID as wasn't updated before due to direct objects insertion
    document.max_id = document.objects.len() as u32;

    // Reorder all new Document objects
    document.renumber_objects();

    // Set any Bookmarks to the First child if they are not set to a page
    document.adjust_zero_pages();

    // Set all bookmarks to the PDF Object tree then set the Outlines to the Bookmark content map.
    if let Some(n) = document.build_outline() {
        if let Ok(Object::Dictionary(dict)) = document.get_object_mut(catalog_object.0) {
            dict.set("Outlines", Object::Reference(n));
        }
    }

    document.compress();

    document
}

fn image_to_doc(path: PathBuf) -> Document {
    //let pagesize = None;
    let page_size_option = Some(PageSizeInMm(210.0, 297.0));

    let doc = PdfDocument::empty("Random Document Title");
    let input_img_file = path.to_str().unwrap();
    let img_result = read_image_from_file(input_img_file);
    if let Err(ref e) = img_result {
        println!(
            "{}: cannot read file {}. {}: {}",
            "Warning".yellow(),
            input_img_file.blue().underline(),
            "Error".red(),
            e
        );
        abort();
    };
    let (color_type, mut img) = img_result.unwrap();
    if let Some(page_size) = &page_size_option {
        let image_transform = get_image_transform_for_page_size(&page_size, &img.image);
        let PageSizeInMm(width, height) = page_size;
        let (page, layer_index) = doc.add_page(
            Mm(width.to_owned() as f32),
            Mm(height.to_owned() as f32),
            "Layer1",
        );
        let current_layer = doc.get_page(page).get_layer(layer_index);
        img.remove_alpha(color_type);
        img.add_to_layer(current_layer.clone(), image_transform);
    } else {
        let (original_image_width, original_image_height) = get_image_dimension_in_mm(&img.image);

        let image_scale = max(
            1,
            max(
                (MIN_WIDTH_IN_MM / original_image_width) as i32,
                (MIN_HEIGHT_IN_MM / original_image_height) as i32,
            ),
        ) as f64;
        let (page, layer_index) = doc.add_page(
            Mm((original_image_width * image_scale) as f32),
            Mm((original_image_height * image_scale) as f32),
            "Layer1",
        );
        let current_layer = doc.get_page(page).get_layer(layer_index);
        img.remove_alpha(color_type);
        img.add_to_layer(
            current_layer.clone(),
            ImageTransform {
                scale_x: Some(image_scale as f32),
                scale_y: Some(image_scale as f32),
                ..Default::default()
            },
        );
    };

    let bytes = doc.save_to_bytes();
    let image_doc = Document::load_mem(bytes.unwrap().as_slice()).unwrap();
    image_doc
}

fn main() {
    // Collect all command-line arguments into a vector
    let args: Vec<String> = env::args().collect();

    // Check if the user requested help
    if args.contains(&"--help".to_string()) || args.contains(&"-h".to_string()) {
        usage();
        process::exit(0); // Exit with a zero status code (successful termination)
    }

    let input_path;
    let output_path;

    if args.len() != 3 {
        // Open a directory picker dialog
        match FileDialog::new().pick_folder() {
            Some(path) => {
                println!("Selected directory: {}", path.display());
                input_path = path;
            }
            None => {
                eprintln!("No directory was selected.");
                exit(1);
            }
        }

        // Open a directory picker dialog
        match FileDialog::new().save_file() {
            Some(path) => {
                println!("Selected directory: {}", path.display());
                output_path = path;
            }
            None => {
                eprintln!("No directory was selected.");
                exit(1);
            }
        }
    } else {
        // Extract the two arguments
        let arg1 = &args[1];
        let arg2 = &args[2];
        input_path = Path::new(arg1.as_str()).to_path_buf();
        output_path = Path::new(arg2.as_str()).to_path_buf();
    }

    let input_path_glob = format!(
        "{}/*.pdf",
        input_path.canonicalize().unwrap().to_str().unwrap()
    );
    let mut input_documents: Vec<Document> = Vec::new();
    for entry in glob(input_path_glob.as_str()).expect("Failed to read glob pattern") {
        match entry {
            Ok(path) => input_documents.push(Document::load(path).unwrap()),
            Err(e) => println!("{:?}", e),
        }
    }

    let input_path_glob_png = format!(
        "{}/*.png",
        input_path.canonicalize().unwrap().to_str().unwrap()
    );

    for entry in glob(input_path_glob_png.as_str()).expect("Failed to read glob pattern") {
        match entry {
            Ok(path) => input_documents.push(image_to_doc(path)),
            Err(e) => println!("{:?}", e),
        }
    }

    let input_path_glob_jpg = format!(
        "{}/*.jpg",
        input_path.canonicalize().unwrap().to_str().unwrap()
    );

    for entry in glob(input_path_glob_jpg.as_str()).expect("Failed to read glob pattern") {
        match entry {
            Ok(path) => input_documents.push(image_to_doc(path)),
            Err(e) => println!("{:?}", e),
        }
    }

    let input_path_glob_jpeg = format!(
        "{}/*.jpeg",
        input_path.canonicalize().unwrap().to_str().unwrap()
    );

    for entry in glob(input_path_glob_jpeg.as_str()).expect("Failed to read glob pattern") {
        match entry {
            Ok(path) => input_documents.push(image_to_doc(path)),
            Err(e) => println!("{:?}", e),
        }
    }

    let input_path_glob_bmp = format!(
        "{}/*.bmp",
        input_path.canonicalize().unwrap().to_str().unwrap()
    );

    for entry in glob(input_path_glob_bmp.as_str()).expect("Failed to read glob pattern") {
        match entry {
            Ok(path) => input_documents.push(image_to_doc(path)),
            Err(e) => println!("{:?}", e),
        }
    }

    // merge the pdfs
    let mut document = merge_documents(input_documents);

    // Save the merged PDF.
    document.save(output_path).unwrap();
}
